import { joinLink, type SyncSettings } from '@moul-dev/client';
import { css, html, LitElement, nothing } from 'lit';
import { unsafeSVG } from 'lit/directives/unsafe-svg.js';
import { copyText } from '../components/records/record-helpers';
import { fieldValue, submitOnEnter } from '../lib/forms';
import { onPress } from '../lib/press';
import { qrSvg } from '../lib/qr';
import { StoreController } from '../lib/store';
import { toast } from '../lib/toast';
import { databaseState } from '../state/database';
import { joinState } from '../state/join';
import { moul } from '../studio';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

/** The CORS rule a bucket needs so pages on any address can sync with it. */
const CORS_RULE = JSON.stringify(
  [
    {
      AllowedOrigins: ['*'],
      AllowedMethods: ['GET', 'PUT', 'HEAD', 'DELETE'],
      AllowedHeaders: ['*'],
      ExposeHeaders: ['ETag'],
      MaxAgeSeconds: 3600,
    },
  ],
  null,
  2,
);

const EMPTY: SyncSettings = { endpoint: '', bucket: '', prefix: '', region: '', accessKeyId: '', secretAccessKey: '', pathStyle: true, label: '' };

function guessLabel(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua)) return 'iPad';
  if (/Android/.test(ua)) return 'Android';
  if (/Mac/.test(ua)) return 'Mac';
  if (/Windows/.test(ua)) return 'Windows PC';
  if (/Linux/.test(ua)) return 'Linux';
  return 'This device';
}

const ago = (ms: number | null | undefined) => {
  if (!ms) return 'never';
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 5) return 'just now';
  if (s < 60) return `${s} s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  return new Date(ms).toLocaleString();
};

/**
 * Sync settings: point the database at an S3-compatible bucket (Cloudflare R2, Amazon S3, MinIO),
 * see which device writes, hand writing over, add another device with a link or QR code, and deal
 * with conflict copies. The replica is the one Litestream writes, so `litestream restore` reads it.
 */
export class AdminSyncPage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      .grid {
        display: grid;
        grid-template-columns: repeat(auto-fill, minmax(min(360px, 100%), 1fr));
        gap: var(--spacing-4);
      }
      form {
        display: flex;
        flex-direction: column;
        gap: var(--spacing-3);
        margin-top: var(--spacing-3);
      }
      .row {
        display: grid;
        grid-template-columns: 1fr 1fr;
        gap: var(--spacing-3);
      }
      @media (max-width: 600px) {
        .row {
          grid-template-columns: 1fr;
        }
      }
      dl {
        display: grid;
        grid-template-columns: max-content 1fr;
        gap: var(--spacing-2) var(--spacing-4);
        margin: var(--spacing-3) 0 0;
        font-size: var(--font-size-sm);
      }
      dt {
        color: var(--color-fg-subtle);
      }
      dd {
        margin: 0;
        word-break: break-word;
      }
      p,
      li {
        font-size: var(--font-size-sm);
        color: var(--color-fg-subtle);
      }
      p {
        margin: var(--spacing-2) 0;
      }
      ol {
        margin: var(--spacing-2) 0;
        padding-left: 1.25rem;
      }
      pre {
        margin: var(--spacing-2) 0;
        padding: var(--spacing-3);
        border-radius: var(--radius-md, 8px);
        background: var(--color-bg-subtle);
        font-family: var(--font-mono);
        font-size: 0.75rem;
        overflow: auto;
      }
      .actions {
        display: flex;
        flex-wrap: wrap;
        gap: var(--spacing-2);
        margin-top: var(--spacing-4);
      }
      .qr {
        width: min(240px, 100%);
        margin-top: var(--spacing-3);
      }
      .qr svg {
        width: 100%;
        height: auto;
        display: block;
        border-radius: 8px;
      }
      .link {
        font-family: var(--font-mono);
        font-size: 0.75rem;
        word-break: break-all;
      }
      .conflict {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: var(--spacing-2);
        padding: var(--spacing-2) 0;
        border-bottom: 1px solid var(--color-border-subtle);
        font-family: var(--font-mono);
        font-size: 0.75rem;
      }
    `,
  ];

  static override properties = {
    form: { state: true },
    fromLink: { state: true },
    confirmJoin: { state: true },
    busy: { state: true },
    secret: { state: true },
    link: { state: true },
    conflicts: { state: true },
  };

  declare form: SyncSettings;
  /**
   * Whether the form holds settings from a join link. Those connect only to follow the bucket: a
   * link must not make this device the writer and upload its data to a bucket the link chose.
   */
  declare fromLink: boolean;
  declare confirmJoin: boolean;
  declare busy: string | undefined;
  /** The secret key, kept only while this page is open, for making a join link. */
  declare secret: string;
  declare link: string | undefined;
  declare conflicts: string[];

  constructor() {
    super();
    this.form = { ...EMPTY, label: guessLabel() };
    this.fromLink = false;
    this.confirmJoin = false;
    this.secret = '';
    this.conflicts = [];
    new StoreController(this, databaseState, joinState);
  }

  override connectedCallback(): void {
    super.connectedCallback();
    void this.loadConflicts();
  }

  override willUpdate(): void {
    // A join link Studio opened with, or one opened in this tab while the page shows.
    const join = joinState.take();
    if (join) this.fillFromLink(join);
  }

  /** Fills the form with a link's bucket settings, and nothing else the link may carry. */
  private fillFromLink(join: SyncSettings) {
    const text = (v: unknown) => (typeof v === 'string' ? v : '');
    this.form = {
      ...EMPTY,
      endpoint: text(join.endpoint),
      bucket: text(join.bucket),
      prefix: text(join.prefix),
      region: text(join.region),
      accessKeyId: text(join.accessKeyId),
      secretAccessKey: text(join.secretAccessKey),
      pathStyle: join.pathStyle !== false,
      label: guessLabel(),
    };
    this.fromLink = true;
    this.confirmJoin = false;
  }

  private clearLink() {
    this.form = { ...EMPTY, label: this.form.label };
    this.fromLink = false;
  }

  private async loadConflicts() {
    this.conflicts = await moul().sync.conflicts().catch(() => []);
  }

  private set(key: keyof SyncSettings, value: string | boolean) {
    this.form = { ...this.form, [key]: value };
  }

  private async run(what: string, action: () => Promise<unknown>, done?: string) {
    this.busy = what;
    try {
      await action();
      if (done) toast.success(done);
    } catch (e) {
      toast.error(`${what} failed`, e instanceof Error ? e.message : String(e));
    } finally {
      this.busy = undefined;
      await databaseState.refresh();
      await this.loadConflicts();
    }
  }

  private async connect(event: Event) {
    event.preventDefault();
    const f = this.form;
    if (!f.endpoint || !f.bucket || !f.accessKeyId || !f.secretAccessKey) {
      toast.warning('Fill in the bucket', 'The endpoint, bucket and both keys are needed.');
      return;
    }
    // Settings from a link connect only once the person agrees to what happens to this device's data.
    if (this.fromLink) this.confirmJoin = true;
    else await this.configure(false);
  }

  private async configure(followOnly: boolean) {
    this.confirmJoin = false;
    const f = this.form;
    const settings: SyncSettings = { ...f, endpoint: f.endpoint.trim(), bucket: f.bucket.trim() };
    if (followOnly) settings.followOnly = true;
    await this.run('Connecting', async () => {
      await moul().sync.configure(settings);
      this.secret = f.secretAccessKey;
      this.form = { ...EMPTY, label: f.label };
      this.fromLink = false;
    }, followOnly ? 'Sync is on: this device follows the bucket' : 'Sync is on');
  }

  /** Where a link's settings point: the bucket, its folder, and the endpoint. */
  private joinTarget() {
    const f = this.form;
    const bucket = `${f.bucket.trim()}${f.prefix ? `/${f.prefix}` : ''}`;
    return html`<strong class="mono">${bucket}</strong> at <strong class="mono">${f.endpoint.trim()}</strong>`;
  }

  private renderJoinDialog() {
    return html`<moul-alert-dialog
      tone="warning"
      size="sm"
      ?is-open=${this.confirmJoin}
      @openchange=${(e: CustomEvent<{ isOpen: boolean }>) => {
        if (!e.detail.isOpen) this.confirmJoin = false;
      }}
    >
      <span slot="header">Follow this bucket?</span>
      <div slot="body">
        <p>This device will copy the data in ${this.joinTarget()} and follow it.</p>
        <p>
          The data this device has now is kept as a separate conflict copy, listed on this page, where you can download or
          delete it. Nothing goes up to the bucket until you take over writing.
        </p>
        <p>Connect only if the link came from one of your own devices.</p>
      </div>
      <moul-button slot="footer" variant="outline" @click=${onPress(() => (this.confirmJoin = false))}>Cancel</moul-button>
      <moul-button slot="footer" variant="primary" @click=${onPress(() => this.configure(true))}>Follow the bucket</moul-button>
    </moul-alert-dialog>`;
  }

  private makeLink() {
    const settings = databaseState.sync?.settings;
    if (!settings || !this.secret) return;
    const page = new URL(window.location.href);
    page.hash = '';
    this.link = joinLink({ ...settings, secretAccessKey: this.secret } as SyncSettings, page);
  }

  private async exportConflict(name: string) {
    const bytes = await moul().sync.exportConflict(name);
    const url = URL.createObjectURL(new Blob([bytes as BlobPart], { type: 'application/vnd.sqlite3' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = name.endsWith('.db') ? name : `${name}.db`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  }

  private field(key: keyof SyncSettings, label: string, options: { type?: string; description?: string; required?: boolean } = {}) {
    return html`<moul-text-field
      label=${label}
      type=${options.type ?? 'text'}
      description=${options.description ?? ''}
      ?is-required=${Boolean(options.required)}
      .value=${String(this.form[key] ?? '')}
      @input=${(e: Event) => this.set(key, fieldValue(e))}
    ></moul-text-field>`;
  }

  private renderSetup() {
    return html`<div class="grid">
      <moul-card>
        <moul-card-body>
          <div class="card-title"><admin-icon name="cloud-arrow-up" size="18"></admin-icon> Connect a bucket</div>
          <p>
            Sync copies every change to a bucket you own, as a Litestream replica. Your other devices follow it, and the
            bucket is a backup you can restore with <code>moul restore</code> or <code>litestream restore</code>.
          </p>
          ${this.fromLink
            ? html`<p>
                These settings came from a join link: this device will follow the bucket, not write to it.
                <moul-button size="sm" variant="ghost" @click=${onPress(() => this.clearLink())}>Clear them</moul-button>
              </p>`
            : nothing}
          <form @submit=${this.connect} @keydown=${submitOnEnter}>
            ${this.field('endpoint', 'Endpoint', {
              required: true,
              description: 'Such as https://<account>.r2.cloudflarestorage.com',
            })}
            <div class="row">
              ${this.field('bucket', 'Bucket', { required: true })}
              ${this.field('prefix', 'Folder in the bucket', { description: 'Optional: for several databases in one bucket' })}
            </div>
            <div class="row">
              ${this.field('accessKeyId', 'Access key ID', { required: true })}
              ${this.field('secretAccessKey', 'Secret access key', { type: 'password', required: true })}
            </div>
            <div class="row">
              ${this.field('region', 'Region', { description: 'Leave empty for R2; the bucket region for S3' })}
              ${this.field('label', 'Name of this device', { description: 'Shown to your other devices' })}
            </div>
            <moul-checkbox ?is-checked=${this.form.pathStyle !== false} @change=${(e: CustomEvent<{ isChecked: boolean }>) => this.set('pathStyle', e.detail.isChecked)}
              >Path-style addresses (R2, MinIO)</moul-checkbox
            >
            <div class="actions">
              <moul-button type="submit" variant="primary" ?is-pending=${this.busy === 'Connecting'}>Turn on sync</moul-button>
            </div>
          </form>
        </moul-card-body>
      </moul-card>
      ${this.renderHelp()}
    </div>`;
  }

  private renderHelp() {
    return html`<moul-card>
      <moul-card-body>
        <div class="card-title"><admin-icon name="info" size="18"></admin-icon> Setting up a bucket</div>
        <ol>
          <li>Make a bucket (Cloudflare R2, Amazon S3, or anything S3-compatible with conditional writes).</li>
          <li>Make an access key that can read and write <strong>only this bucket</strong>. It is kept in this browser.</li>
          <li>Let pages reach the bucket with this CORS rule (narrow <code>AllowedOrigins</code> to your app's address):</li>
        </ol>
        <pre>${CORS_RULE}</pre>
        <moul-button size="sm" variant="ghost" @click=${onPress(async () => {
          if (await copyText(CORS_RULE)) toast.success('Copied the CORS rule');
        })}>
          <admin-icon slot="prefix" name="copy" size="14"></admin-icon>
          Copy the rule
        </moul-button>
        <p>
          One device writes at a time. The others follow and are read-only until they take over writing. Moul does not
          encrypt the replica: who can read the bucket can read the data.
        </p>
      </moul-card-body>
    </moul-card>`;
  }

  private renderStatus() {
    const s = databaseState.sync!;
    const writer = s.writer;
    const role = s.role === 'writer' ? 'This device writes' : s.role === 'follower' ? 'Following another device' : 'Connecting';
    return html`${this.fromLink
        ? html`<moul-alert
            tone="warning"
            description=${`A join link for the bucket ${this.form.bucket}${this.form.prefix ? `/${this.form.prefix}` : ''} was opened, but this database already syncs. Turn sync off first to follow that bucket instead; the link's settings are kept until then.`}
          ></moul-alert>`
        : nothing}
      <div class="grid">
      <moul-card>
        <moul-card-body>
          <div class="card-title">
            <admin-icon name="arrows-clockwise" size="18"></admin-icon> Sync
            <moul-badge size="sm" tone=${s.lastError ? 'error' : s.connected ? 'success' : 'warning'}
              >${s.lastError ? 'error' : s.connected ? 'connected' : 'offline'}</moul-badge
            >
          </div>
          <dl>
            <dt>Bucket</dt>
            <dd class="mono">${s.settings?.bucket}${s.settings?.prefix ? `/${s.settings.prefix}` : ''}</dd>
            <dt>Endpoint</dt>
            <dd class="mono">${s.settings?.endpoint}</dd>
            <dt>Role</dt>
            <dd>${role}</dd>
            <dt>Writer</dt>
            <dd>${writer ? `${writer.label || writer.device}${writer.device === s.device ? ' (this device)' : ''}` : '…'}</dd>
            <dt>Last sync</dt>
            <dd>${ago(s.lastSyncAt)}</dd>
            <dt>Position</dt>
            <dd>${s.position} here · ${s.remote ?? '?'} in the bucket${s.pending ? ` · ${s.pending} waiting to go up` : ''}</dd>
            ${s.lastError ? html`<dt>Error</dt><dd style="color: var(--color-error-600)">${s.lastError}</dd>` : nothing}
          </dl>
          <div class="actions">
            <moul-button variant="secondary" ?is-pending=${this.busy === 'Syncing'} @click=${onPress(() => this.run('Syncing', () => moul().sync.now()))}>
              Sync now
            </moul-button>
            ${s.role === 'follower'
              ? html`<moul-button variant="primary" ?is-pending=${this.busy === 'Taking over'} @click=${onPress(() =>
                  this.run('Taking over', () => moul().sync.takeOver(), 'This device writes now'),
                )}>Take over writing</moul-button>`
              : nothing}
            <moul-button variant="ghost" ?is-pending=${this.busy === 'Disconnecting'} @click=${onPress(() =>
              this.run(
                'Disconnecting',
                () => moul().sync.disconnect(),
                'Sync is off; the data stays here, and changes go up when you connect to this bucket again',
              ),
            )}>Turn off sync</moul-button>
            <moul-button variant="ghost" ?is-pending=${this.busy === 'Leaving'} @click=${onPress(() =>
              this.run('Leaving', () => moul().sync.disconnect({ forget: true }), 'This device left the bucket; the data stays here'),
            )}>Leave this bucket</moul-button>
          </div>
          <p>
            Turning sync off keeps this device's later changes for the next time it connects to this bucket. Leaving the
            bucket stops keeping them; connecting again then starts over.
          </p>
        </moul-card-body>
      </moul-card>

      <moul-card>
        <moul-card-body>
          <div class="card-title"><admin-icon name="link-simple" size="18"></admin-icon> Add another device</div>
          <p>
            Open a link (or scan the code) on your phone or another computer. It carries the bucket key, so share it only
            with your own devices.
          </p>
          ${this.link
            ? html`<div class="qr">${unsafeSVG(qrSvg(this.link))}</div>
                <p class="link">${this.link}</p>
                <div class="actions">
                  <moul-button size="sm" variant="secondary" @click=${onPress(async () => {
                    if (await copyText(this.link!)) toast.success('Copied the link');
                  })}><admin-icon slot="prefix" name="copy" size="14"></admin-icon>Copy the link</moul-button>
                  <moul-button size="sm" variant="ghost" @click=${() => (this.link = undefined)}>Hide</moul-button>
                </div>`
            : html`<form @submit=${(e: Event) => {
                e.preventDefault();
                this.makeLink();
              }} @keydown=${submitOnEnter}>
                <moul-text-field
                  label="Secret access key"
                  type="password"
                  description="The key is not shown again after connecting; enter it to put it in the link."
                  .value=${this.secret}
                  @input=${(e: Event) => (this.secret = fieldValue(e))}
                ></moul-text-field>
                <div class="actions">
                  <moul-button type="submit" variant="secondary" ?is-disabled=${!this.secret}>Make a link</moul-button>
                </div>
              </form>`}
        </moul-card-body>
      </moul-card>

      ${this.conflicts.length
        ? html`<moul-card>
            <moul-card-body>
              <div class="card-title"><admin-icon name="warning-circle" size="18"></admin-icon> Conflict copies</div>
              <p>
                This device had changes the bucket did not when another device took over writing. They were kept here as
                separate copies; nothing was discarded.
              </p>
              ${this.conflicts.map(
                (name) => html`<div class="conflict">
                  <span>${name}</span>
                  <span>
                    <moul-button size="sm" variant="ghost" @click=${onPress(() => this.exportConflict(name))}>Download</moul-button>
                    <moul-button size="sm" variant="ghost" @click=${onPress(() =>
                      this.run('Deleting the copy', () => moul().sync.deleteConflict(name)),
                    )}>Delete</moul-button>
                  </span>
                </div>`,
              )}
            </moul-card-body>
          </moul-card>`
        : nothing}
    </div>`;
  }

  override render() {
    const s = databaseState.sync;
    return html`
      ${pageHeader('Sync', 'Keep this database on your other devices, through a bucket you own.')}
      ${!s ? html`<div class="muted">Loading…</div>` : s.configured ? this.renderStatus() : this.renderSetup()}
      ${this.renderJoinDialog()}
    `;
  }
}

customElements.define('admin-sync-page', AdminSyncPage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-sync-page': AdminSyncPage;
  }
}
