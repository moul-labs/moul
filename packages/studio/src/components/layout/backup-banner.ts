import { css, html, LitElement, nothing } from 'lit';
import { onPress } from '../../lib/press';
import { router } from '../../lib/router';
import { StoreController } from '../../lib/store';
import { databaseState } from '../../state/database';
import { moul } from '../../studio';
import { baseStyles } from '../../styles/base';

const isIos = () =>
  /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
const isStandalone = () =>
  window.matchMedia?.('(display-mode: standalone)').matches || (navigator as any).standalone === true;

/**
 * The line under the header that says when the data is at risk: kept in memory only, not backed
 * up, or in storage the browser may clear. Every screen has it, with the fix one click away.
 */
export class AdminBackupBanner extends LitElement {
  static override styles = [
    baseStyles,
    css`
      :host {
        display: block;
      }
      .banner {
        display: flex;
        align-items: center;
        gap: var(--spacing-3);
        flex-wrap: wrap;
        padding: var(--spacing-2) var(--spacing-4);
        font-size: 0.8125rem;
        border-bottom: 1px solid var(--color-border);
        background: light-dark(#fffbeb, #2a2210);
        color: light-dark(#78350f, #fde68a);
      }
      .banner.danger {
        background: light-dark(#fef2f2, #2b1111);
        color: light-dark(#7f1d1d, #fecaca);
      }
      .text {
        flex: 1 1 18rem;
        min-width: 0;
      }
      .actions {
        display: flex;
        gap: var(--spacing-2);
        flex-wrap: wrap;
      }
    `,
  ];

  private dismissed = false;

  constructor() {
    super();
    new StoreController(this, databaseState);
  }

  private async download() {
    await databaseState.download(moul());
  }

  override render() {
    const s = databaseState.status;
    if (!s || this.dismissed) return nothing;
    if (!s.durable) {
      return html`<div class="banner danger" role="status">
        <admin-icon name="warning-circle" size="16"></admin-icon>
        <span class="text"
          >This browser is not keeping the data: ${s.note ?? 'it runs in memory'}. Everything here goes when the tab
          closes. Download the file to keep it.</span
        >
        <span class="actions">
          <moul-button size="sm" variant="secondary" @click=${onPress(() => this.download())}>Download the file</moul-button>
        </span>
      </div>`;
    }
    if (!databaseState.backedUp && databaseState.hasData) {
      const tip = isIos() && !isStandalone() ? ' On iPhone and iPad, add this page to the Home Screen so Safari keeps it.' : '';
      return html`<div class="banner" role="status">
        <admin-icon name="warning-circle" size="16"></admin-icon>
        <span class="text"
          >Not backed up: this data lives only in this browser${databaseState.exportedAt !== undefined
            ? ', and it changed since your last download'
            : ''}.${tip}</span
        >
        <span class="actions">
          <moul-button size="sm" variant="secondary" @click=${onPress(() => this.download())}>Download the file</moul-button>
          <moul-button size="sm" variant="ghost" @click=${() => router.navigate('/sync')}>Turn on sync</moul-button>
          <moul-button size="sm" variant="ghost" @click=${() => {
            this.dismissed = true;
            this.requestUpdate();
          }}>Later</moul-button>
        </span>
      </div>`;
    }
    if (databaseState.persisted === false && databaseState.hasData) {
      return html`<div class="banner" role="status">
        <admin-icon name="info" size="16"></admin-icon>
        <span class="text">The browser may clear this site's storage when space runs low.</span>
        <span class="actions">
          <moul-button size="sm" variant="secondary" @click=${onPress(async () => {
            await moul().database.persist();
            await databaseState.refresh();
          })}>Ask the browser to keep it</moul-button>
        </span>
      </div>`;
    }
    return nothing;
  }
}

customElements.define('admin-backup-banner', AdminBackupBanner);

declare global {
  interface HTMLElementTagNameMap {
    'admin-backup-banner': AdminBackupBanner;
  }
}
