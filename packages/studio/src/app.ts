import { css, html, LitElement, nothing } from 'lit';
import { keyed } from 'lit/directives/keyed.js';
import { logoIcon } from './components/layout/logo';
import { onPress } from './lib/press';
import { type Route, router } from './lib/router';
import { StoreController } from './lib/store';
import { studio } from './studio';
import { baseStyles } from './styles/base';

type LoadState = 'loading' | 'ready' | 'failed';

/**
 * Root element: once Studio has a database, renders the routed page, inside `<admin-layout>`
 * for app routes. A page element is kept while only params or
 * search change, and replaced when the route changes. When Studio moves to another client, the
 * layout and the page are made again, so nothing stays bound to the last database.
 */
export class AdminApp extends LitElement {
  static override styles = [
    baseStyles,
    css`
      :host {
        display: block;
        height: 100%;
        width: 100%;
      }

      .screen {
        display: flex;
        flex-direction: column;
        align-items: center;
        justify-content: center;
        height: 100%;
        width: 100%;
        padding: var(--spacing-6);
        gap: var(--spacing-3);
        background-color: var(--color-bg-subtle);
        color: var(--color-fg-subtle);
        font-family: var(--font-family-base);
        text-align: center;
      }

      .screen-logo {
        color: var(--color-fg);
      }

      h2 {
        margin: 0;
        color: var(--color-error-500);
      }

      p {
        margin: 0 0 var(--spacing-3);
        max-width: 600px;
      }
    `,
  ];

  private readonly loads = new Map<Route, LoadState>();
  private loadError: unknown = null;
  private pageRoute: Route | undefined;
  private pageClient: unknown;
  private pageElement: HTMLElement | undefined;

  constructor() {
    super();
    new StoreController(this, router, studio);
  }

  private ensureLoaded(route: Route): LoadState {
    const state = this.loads.get(route);
    if (state) return state;
    if (!route.load) return 'ready';

    this.loads.set(route, 'loading');
    route.load().then(
      () => {
        this.loads.set(route, 'ready');
        this.requestUpdate();
      },
      (err) => {
        this.loads.set(route, 'failed');
        this.loadError = err;
        this.requestUpdate();
      },
    );
    return 'loading';
  }

  private pageFor(route: Route & { tag: string }): HTMLElement {
    if (this.pageRoute !== route || this.pageClient !== studio.client || !this.pageElement) {
      this.pageRoute = route;
      this.pageClient = studio.client;
      this.pageElement = document.createElement(route.tag);
    }
    return this.pageElement;
  }

  private renderPending() {
    return html`
      <div class="screen">
        <span class="screen-logo">${logoIcon(32)}</span>
        <span style="font-size: 0.875rem">Opening the database…</span>
      </div>
    `;
  }

  private renderError(message: string) {
    return html`
      <div class="screen">
        <h2>Studio could not load this page</h2>
        <p>${message}</p>
        <moul-button variant="primary" @click=${onPress(() => window.location.reload())}>Reload</moul-button>
      </div>
    `;
  }

  private renderNotFound() {
    return html`
      <div class="screen">
        <span class="screen-logo">${logoIcon(32)}</span>
        <p>No Studio page lives at this address.</p>
        <moul-button variant="primary" @click=${onPress(() => router.navigate('/overview'))}>Go to Overview</moul-button>
      </div>
    `;
  }

  override render() {
    if (!studio.client) return this.renderPending();

    const { route } = router.location;
    if (!route?.tag) return this.renderNotFound();

    const state = this.ensureLoaded(route);
    if (state === 'failed') {
      const message = this.loadError instanceof Error ? this.loadError.message : '';
      return this.renderError(message || 'An unexpected error occurred in Studio.');
    }

    const content = state === 'ready' ? this.pageFor(route as Route & { tag: string }) : nothing;
    if (route.layout === 'app') {
      return keyed(studio.client, html`<admin-layout>${content}</admin-layout>`);
    }
    return state === 'ready' ? content : this.renderPending();
  }
}

customElements.define('admin-app', AdminApp);

declare global {
  interface HTMLElementTagNameMap {
    'admin-app': AdminApp;
  }
}
