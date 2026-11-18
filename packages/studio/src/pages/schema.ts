import { css, html, LitElement, nothing } from 'lit';
import { copyText } from '../components/records/record-helpers';
import { MoulLiveController } from '../lib/moul-live';
import { onPress } from '../lib/press';
import { collectionsPrunedBy } from '../lib/schema-file';
import { toast } from '../lib/toast';
import { moul } from '../studio';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

function save(text: string, name: string, type: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

const SDK_SNIPPET = `import { createClient } from '@moul-dev/client';
import type { MoulSchema } from './moul-types';

const moul = createClient<MoulSchema>({ db: '<name>' });

// Every read can stay current: it refreshes when anything it read changes.
moul.collection('<collection>').live({ sort: '-createdAt' }).subscribe(({ data }) => {
  console.log(data?.items);
});`;

/**
 * The app's definition as files: the schema file (collections, fields and rules) for git, review
 * and AI tools, and TypeScript types for the SDK. A schema file can be applied here too.
 */
export class AdminSchemaPage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      .grid {
        display: grid;
        grid-template-columns: repeat(auto-fill, minmax(min(420px, 100%), 1fr));
        gap: var(--spacing-4);
      }
      p {
        margin: var(--spacing-2) 0;
        font-size: var(--font-size-sm);
        color: var(--color-fg-subtle);
      }
      pre {
        margin: var(--spacing-3) 0 0;
        padding: var(--spacing-3);
        max-height: 420px;
        overflow: auto;
        border-radius: var(--radius-md, 8px);
        background: var(--color-bg-subtle);
        font-family: var(--font-mono);
        font-size: 0.75rem;
        white-space: pre;
      }
      .actions {
        display: flex;
        flex-wrap: wrap;
        gap: var(--spacing-2);
        margin-top: var(--spacing-3);
      }
      ul {
        margin: var(--spacing-2) 0;
        padding-left: 1.25rem;
        font-size: var(--font-size-sm);
      }
      code {
        font-family: var(--font-mono);
      }
    `,
  ];

  static override properties = { prune: { state: true }, applying: { state: true }, confirmPrune: { state: true } };
  declare prune: boolean;
  declare applying: boolean;
  /** A schema file waiting for the person to agree to the collections applying it deletes. */
  declare confirmPrune: { schema: unknown; file: string; deleted: string[] } | undefined;

  constructor() {
    super();
    this.prune = false;
    this.applying = false;
    this.confirmPrune = undefined;
  }

  private readonly schema = new MoulLiveController(this, () =>
    moul().liveRequest<object>({ path: '/api/schema', owner: true }),
  );
  private readonly types = new MoulLiveController(this, () =>
    moul().live(async () => ({ data: await moul().admin.types(), reads: { all: false, ids: new Set(), names: new Set() } })),
  );

  private apply() {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.json,application/json';
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      const prune = this.prune;
      this.applying = true;
      try {
        const schema = JSON.parse(await file.text());
        if (prune) {
          // Deleting collections deletes their records: name them and ask first.
          const deleted = collectionsPrunedBy(schema, await moul().admin.listCollections());
          if (deleted.length > 0) {
            this.confirmPrune = { schema, file: file.name, deleted };
            return;
          }
        }
        await this.send(schema, prune);
      } catch (e) {
        toast.error('The schema was not applied', e instanceof Error ? e.message : String(e));
      } finally {
        this.applying = false;
      }
    };
    input.click();
  }

  private async send(schema: unknown, prune: boolean) {
    const result = await moul().admin.applySchema(schema as object, { prune });
    const n = (k: string) => (Array.isArray(result?.[k]) ? result[k].length : 0);
    toast.success('Schema applied', `${n('created')} created, ${n('updated')} changed, ${n('deleted')} deleted`);
  }

  private async applyConfirmed() {
    const pending = this.confirmPrune;
    this.confirmPrune = undefined;
    if (!pending) return;
    this.applying = true;
    try {
      // Another tab may have made a collection meanwhile: ask again rather than delete it unseen.
      const deleted = collectionsPrunedBy(pending.schema, await moul().admin.listCollections());
      if (deleted.some((name) => !pending.deleted.includes(name))) {
        this.confirmPrune = { ...pending, deleted };
        return;
      }
      await this.send(pending.schema, true);
    } catch (e) {
      toast.error('The schema was not applied', e instanceof Error ? e.message : String(e));
    } finally {
      this.applying = false;
    }
  }

  override render() {
    const schemaText = this.schema.state.data ? JSON.stringify(this.schema.state.data, null, 2) : '';
    const typesText = this.types.state.data ?? '';
    return html`
      ${pageHeader('Schema & types', 'The app’s definition as files, for git, code review, TypeScript and AI tools.')}
      <div class="grid">
        <moul-card>
          <moul-card-body>
            <div class="card-title"><admin-icon name="brackets-curly" size="18"></admin-icon> Schema file</div>
            <p>
              Collections, fields and rules in one file. Keep it in your app's repository as <code>moul.schema.json</code>;
              <code>moul check</code> validates it and the SDK can apply it when the app opens.
            </p>
            <div class="actions">
              <moul-button size="sm" variant="secondary" ?is-disabled=${!schemaText} @click=${onPress(() =>
                save(schemaText + '\n', 'moul.schema.json', 'application/json'),
              )}><admin-icon slot="prefix" name="download-simple" size="14"></admin-icon>Download</moul-button>
              <moul-button size="sm" variant="ghost" ?is-disabled=${!schemaText} @click=${onPress(async () => {
                if (await copyText(schemaText)) toast.success('Copied the schema');
              })}><admin-icon slot="prefix" name="copy" size="14"></admin-icon>Copy</moul-button>
              <moul-button size="sm" variant="outline" ?is-pending=${this.applying} @click=${onPress(() => this.apply())}>
                <admin-icon slot="prefix" name="upload-simple" size="14"></admin-icon>Apply a schema file…
              </moul-button>
              <moul-checkbox ?is-checked=${this.prune} @change=${(e: CustomEvent<{ isChecked: boolean }>) => (this.prune = e.detail.isChecked)}
                >Delete collections it does not list</moul-checkbox
              >
            </div>
            ${schemaText ? html`<pre>${schemaText}</pre>` : nothing}
          </moul-card-body>
        </moul-card>

        <moul-card>
          <moul-card-body>
            <div class="card-title"><admin-icon name="file-code" size="18"></admin-icon> TypeScript types</div>
            <p>
              Record types for <code>@moul-dev/client</code>, the same file <code>moul typegen</code> writes. Save it as
              <code>moul-types.ts</code> and pass <code>MoulSchema</code> to <code>createClient</code>.
            </p>
            <div class="actions">
              <moul-button size="sm" variant="secondary" ?is-disabled=${!typesText} @click=${onPress(() =>
                save(typesText, 'moul-types.ts', 'text/plain'),
              )}><admin-icon slot="prefix" name="download-simple" size="14"></admin-icon>Download</moul-button>
              <moul-button size="sm" variant="ghost" ?is-disabled=${!typesText} @click=${onPress(async () => {
                if (await copyText(typesText)) toast.success('Copied the types');
              })}><admin-icon slot="prefix" name="copy" size="14"></admin-icon>Copy</moul-button>
            </div>
            ${typesText ? html`<pre>${typesText}</pre>` : nothing}
          </moul-card-body>
        </moul-card>
      </div>

      <moul-card>
        <moul-card-body>
          <div class="card-title"><admin-icon name="code" size="18"></admin-icon> Use it from your app</div>
          <p>
            Install <code>@moul-dev/client</code>. The engine starts inside your app's page, on the same data Studio shows
            when both are on one site. AI tools can read <code>node_modules/@moul-dev/client/llms.txt</code>.
          </p>
          <pre>${SDK_SNIPPET}</pre>
        </moul-card-body>
      </moul-card>

      <moul-alert-dialog
        tone="error"
        size="sm"
        ?is-open=${Boolean(this.confirmPrune)}
        @openchange=${(e: CustomEvent<{ isOpen: boolean }>) => {
          if (!e.detail.isOpen) this.confirmPrune = undefined;
        }}
      >
        <span slot="header">Delete collections?</span>
        <div slot="body">
          <p>
            <strong>${this.confirmPrune?.file}</strong> does not list these collections, so applying it deletes them and
            all their records:
          </p>
          <ul>
            ${(this.confirmPrune?.deleted ?? []).map((name) => html`<li><code>${name}</code></li>`)}
          </ul>
          <p>This cannot be undone. Download the database file first if you may want them back.</p>
        </div>
        <moul-button slot="footer" variant="outline" @click=${onPress(() => (this.confirmPrune = undefined))}>Cancel</moul-button>
        <moul-button slot="footer" variant="danger" @click=${onPress(() => this.applyConfirmed())}>Apply and delete</moul-button>
      </moul-alert-dialog>
    `;
  }
}

customElements.define('admin-schema-page', AdminSchemaPage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-schema-page': AdminSchemaPage;
  }
}
