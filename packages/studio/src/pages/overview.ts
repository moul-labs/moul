import { css, html, LitElement } from 'lit';
import { repeat } from 'lit/directives/repeat.js';
import { api } from '../api/client';
import { moulsCollection } from '../api/db';
import { collectionTypeBadge } from '../components/collections/collection-type';
import { LiveQueryController } from '../lib/live-query';
import { onPress } from '../lib/press';
import { QueryController } from '../lib/query';
import { router } from '../lib/router';
import { StoreController } from '../lib/store';
import { databaseState } from '../state/database';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

const RECORDS_SEARCH = { page: 1, perPage: 30 };

/** One collection on the overview: name, type, live record count and quick actions. */
export class AdminCollectionCard extends LitElement {
  static override styles = [
    baseStyles,
    css`
      :host {
        display: block;
      }

      moul-card {
        height: 100%;
      }

      .inner {
        display: flex;
        flex-direction: column;
        gap: var(--spacing-3);
        width: 100%;
      }

      .top-row {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: var(--spacing-2);
      }

      .name {
        display: flex;
        align-items: center;
        gap: var(--spacing-2);
        min-width: 0;
        font-weight: 600;
        font-size: 0.9375rem;
        color: var(--color-fg);
        text-decoration: none;
        text-transform: capitalize;
        border-radius: var(--radius-sm);
      }

      .name:hover {
        text-decoration: underline;
      }

      .name:focus-visible {
        outline: none;
        box-shadow: var(--focus-ring);
      }

      .badges {
        display: flex;
        align-items: center;
        gap: var(--spacing-2);
        flex-shrink: 0;
      }

      .bottom-row {
        display: flex;
        align-items: center;
        justify-content: space-between;
        padding-top: var(--spacing-2);
        border-top: 1px solid var(--color-border-subtle);
      }
    `,
  ];

  static override properties = {
    moul: { attribute: false },
  };

  declare moul: { name: string; type: string };

  private readonly count = new QueryController(this, () => ({
    queryKey: ['collectionRecordCount', this.moul?.name],
    queryFn: () => api.listRecords(this.moul.name, { perPage: 1 }),
    enabled: Boolean(this.moul?.name),
  }));

  override render() {
    const { name, type } = this.moul;
    const { data, isLoading } = this.count.result;
    const totalCount =
      (data && !Array.isArray(data) ? data.totalItems : undefined) ?? (Array.isArray(data) ? data.length : 0);
    const recordsPath = `/records/${encodeURIComponent(name)}`;

    return html`
      <moul-card>
        <moul-card-body>
          <div class="inner">
            <div class="top-row">
              <a class="name" href=${router.href(recordsPath, RECORDS_SEARCH)}>
                <admin-icon name="database" size="18" color="var(--color-primary-500)"></admin-icon>
                <span>${name}</span>
              </a>
              <div class="badges">
                ${collectionTypeBadge(type)}
                <moul-badge variant="bordered">${isLoading ? '...' : `${totalCount} records`}</moul-badge>
              </div>
            </div>

            <div class="bottom-row">
              <moul-button
                variant="outline"
                aria-label=${`Create new record in ${name}`}
                @click=${onPress(() => router.navigate(recordsPath, { search: { ...RECORDS_SEARCH, create: true } }))}
              >
                <admin-icon slot="prefix" name="plus" size="14"></admin-icon>
                New Record
              </moul-button>
              <moul-button
                variant="ghost"
                aria-label=${`Browse ${name} records`}
                @click=${onPress(() => router.navigate(recordsPath, { search: RECORDS_SEARCH }))}
              >
                View Table
                <admin-icon slot="suffix" name="arrow-right" size="14"></admin-icon>
              </moul-button>
            </div>
          </div>
        </moul-card-body>
      </moul-card>
    `;
  }
}

customElements.define('admin-collection-card', AdminCollectionCard);

/** Landing page: where the database stands and a card per collection. */
export class AdminOverviewPage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      .stats {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
        gap: var(--spacing-4);
      }

      .section {
        display: flex;
        flex-direction: column;
        gap: var(--spacing-4);
      }

      .section-header {
        display: flex;
        align-items: center;
        justify-content: space-between;
      }

      h2 {
        margin: 0;
        font-size: 1.125rem;
        font-weight: 600;
      }

      .collections {
        display: grid;
        grid-template-columns: repeat(auto-fill, minmax(min(340px, 100%), 1fr));
        gap: var(--spacing-4);
      }
    `,
  ];

  private readonly mouls = new LiveQueryController(this, (q) => q.from({ moul: moulsCollection }));

  constructor() {
    super();
    new StoreController(this, databaseState);
  }

  private renderCollections() {
    const { data: mouls, isLoading } = this.mouls;
    if (isLoading) {
      return html`<div class="muted">Loading collections...</div>`;
    }
    if (mouls.length === 0) {
      return html`
        <moul-empty-state variant="bordered">
          <admin-icon slot="icon" name="database" size="32" color="var(--color-primary-500)"></admin-icon>
          <span slot="title">No collections yet</span>
          <span slot="description">Create your first collection to define your schema and store data.</span>
          <moul-button slot="actions" variant="primary" @click=${onPress(() => router.navigate('/collections'))}>
            <admin-icon slot="prefix" name="plus" size="14"></admin-icon>
            Create Collection
          </moul-button>
        </moul-empty-state>
      `;
    }
    return html`
      <div class="collections">
        ${repeat(
          mouls,
          (moul) => moul.name,
          (moul) => html`<admin-collection-card .moul=${moul}></admin-collection-card>`,
        )}
      </div>
    `;
  }

  override render() {
    const { data: mouls, isLoading: moulsLoading } = this.mouls;
    const collectionCount = Array.isArray(mouls) ? mouls.length : 0;
    const loadingText = () => '...';
    const status = databaseState.status;
    const sync = databaseState.sync;
    const storage = !status ? '...' : status.durable ? (databaseState.persisted ? 'Kept' : 'Best effort') : 'Memory';
    const syncText = !sync ? '...' : !sync.configured ? 'Off' : sync.role === 'writer' ? 'Writing' : sync.role === 'follower' ? 'Following' : 'Connecting';

    return html`
      ${pageHeader(
        'Overview',
        'This database and its collections at a glance.',
        html`
          <moul-button variant="primary" @click=${onPress(() => router.navigate('/collections'))}>
            Manage Collections
            <admin-icon slot="suffix" name="arrow-right" size="16"></admin-icon>
          </moul-button>
        `,
      )}

      <div class="stats">
        <moul-stat
          label="Collections"
          description="Active schema tables"
          .value=${collectionCount}
          .valueFormatter=${moulsLoading ? loadingText : undefined}
        >
          <admin-icon slot="icon" name="database" size="20" color="var(--color-primary-500)"></admin-icon>
        </moul-stat>
        <moul-stat
          label="Storage"
          description=${status?.durable ? 'In this browser (OPFS)' : 'Gone when the tab closes'}
          .value=${1}
          .valueFormatter=${() => storage}
        >
          <admin-icon slot="icon" name="hard-drives" size="20" color="var(--color-success-500)"></admin-icon>
        </moul-stat>
        <moul-stat label="Sync" description=${sync?.configured ? (sync.settings?.bucket ?? 'A bucket') : 'Your other devices'} .value=${1} .valueFormatter=${() => syncText}>
          <admin-icon slot="icon" name="arrows-clockwise" size="20" color="var(--color-warning-500)"></admin-icon>
        </moul-stat>
        <moul-stat label="Commits" description="Written to this database" .value=${status?.lastCommit ?? 0} .valueFormatter=${status ? undefined : loadingText}>
          <admin-icon slot="icon" name="pulse" size="20" color="var(--color-primary-500)"></admin-icon>
        </moul-stat>
      </div>

      <div class="section">
        <div class="section-header">
          <h2>Collections</h2>
          ${collectionCount > 2 ? html`<span class="muted" style="font-size: var(--font-size-sm)">${collectionCount} collections</span>` : ''}
        </div>
        ${this.renderCollections()}
      </div>
    `;
  }
}

customElements.define('admin-overview-page', AdminOverviewPage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-collection-card': AdminCollectionCard;
    'admin-overview-page': AdminOverviewPage;
  }
}
