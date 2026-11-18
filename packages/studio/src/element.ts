// <moul-studio>: the Moul admin console as one element. It works on a database in the page:
// collections, records, rules, the local inbox, the database file and sync.
//
//   <script type="module">
//     import '@moul-dev/studio';
//   </script>
//   <moul-studio db="notes"></moul-studio>
//
// or, sharing the app's own client (and so its engine):
//
//   document.querySelector('moul-studio').client = moul;
import '@moul-dev/web-component';
import tokens from '@moul-dev/web-component/style.css?inline';
import { createClient, type MoulClient } from '@moul-dev/client';
import './icons';
import './components/layout/app-layout';
import './app';
import { followEngine, resetCollections } from './api/db';
import { queryClient } from './lib/query';
import { router, type RoutingMode } from './lib/router';
import { routes } from './routes';
import { scopeTokens } from './lib/tokens';
import { databaseState } from './state/database';
import { joinState } from './state/join';
import { theme } from './state/theme';
import { studio } from './studio';

const STYLE_ID = 'moul-studio-tokens';

/**
 * The design tokens Moul UI's elements read, once per page. They stay on `:root`: Moul UI puts its
 * toasts in `document.body`, outside `<moul-studio>`, and they read the same tokens. The color
 * tokens are computed from `--brand-hue` where they are declared, so they are declared on
 * `<moul-studio>` too, for Studio's own hue to apply inside it. Studio's other variables are set on
 * `<moul-studio>` only, and leave the page around it alone.
 */
function injectTokens(): void {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement('style');
  style.id = STYLE_ID;
  style.textContent = `${scopeTokens(tokens)}
moul-studio {
  color-scheme: light dark;
  --brand-hue: 205;
  --font-sans: "Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  --font-mono: "JetBrains Mono", "Fira Code", ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  --content-max-width: 1200px;
  display: block;
  height: 100%;
  min-height: 480px;
}`;
  document.head.append(style);
}

/**
 * The Moul admin console. Give it a database with the `db` attribute (it then starts the engine
 * itself) or a client with the `client` property (it then shares the app's engine). One per page.
 *
 * Attributes:
 * - `db`: the database name, when no `client` is set. `app` by default.
 * - `routing`: `memory` (default) keeps Studio's page out of the address bar, for Studio inside an
 *   app; `hash` keeps it in the URL's fragment (`#/records/tasks`), for a page that is only Studio.
 */
export class MoulStudio extends HTMLElement {
  static observedAttributes = ['db', 'routing'];
  /** The engine Studio started itself, from `db`. */
  private ownClient: MoulClient | undefined;
  private given: MoulClient | undefined;
  /** The client Studio follows while connected. */
  private followed: MoulClient | undefined;
  private stops: (() => void)[] = [];
  private stopTheme: (() => void) | undefined;
  private connected = false;

  /** The client Studio works through. Setting it shares the app's engine. */
  get client(): MoulClient | undefined {
    return this.given ?? this.ownClient;
  }

  set client(client: MoulClient | undefined) {
    this.given = client;
    if (this.connected) this.follow();
    // Studio's own engine is not needed once the app gives one (unless it gave that one back).
    if (client === this.ownClient) this.ownClient = undefined;
    else if (client) this.closeOwnClient();
  }

  connectedCallback(): void {
    // A `client` set before this element was upgraded is an own property hiding the setter.
    if (Object.hasOwn(this, 'client')) {
      const early = (this as { client?: MoulClient }).client;
      delete (this as { client?: MoulClient }).client;
      this.client = early;
    }
    injectTokens();
    if (!this.shadowRoot) {
      const root = this.attachShadow({ mode: 'open' });
      root.innerHTML = '<style>:host{display:block;height:100%}admin-app{display:block;height:100%}</style><admin-app></admin-app>';
    }
    this.connected = true;
    this.stopTheme = theme.attach(this);
    window.addEventListener('hashchange', this.onHashChange);
    this.startRouter(joinState.capture());
    if (this.given) {
      this.follow();
    } else {
      // The app may set `client` right after the element connects: wait for that before starting
      // an engine of Studio's own.
      queueMicrotask(() => {
        if (this.connected && !this.followed) this.follow();
      });
    }
  }

  disconnectedCallback(): void {
    this.connected = false;
    for (const s of this.stops) s();
    this.stops = [];
    this.followed = undefined;
    window.removeEventListener('hashchange', this.onHashChange);
    this.stopTheme?.();
    this.stopTheme = undefined;
    // Removed for good (not just moved): stop the engine Studio started itself.
    queueMicrotask(() => {
      if (!this.connected) this.closeOwnClient();
    });
  }

  attributeChangedCallback(name: string, old: string | null, value: string | null): void {
    if (old === value) return;
    if (name === 'db' && !this.given) {
      const previous = this.ownClient;
      this.ownClient = undefined;
      if (this.connected && this.followed) this.follow();
      previous?.close();
    } else if (name === 'routing' && this.connected) {
      this.startRouter(false);
    }
  }

  /** A join link opened in this tab while Studio is open: the Sync page takes it. */
  private readonly onHashChange = (): void => {
    if (joinState.capture()) router.navigate('/sync', { replace: true });
  };

  private startRouter(joining: boolean): void {
    const mode: RoutingMode = this.getAttribute('routing') === 'hash' ? 'hash' : 'memory';
    router.start(routes, { mode, root: this, initial: joining ? '/sync' : '/overview' });
    if (joining) router.navigate('/sync', { replace: true });
  }

  private closeOwnClient(): void {
    this.ownClient?.close();
    this.ownClient = undefined;
  }

  /** Points Studio at the given client, or at an engine of its own on `db`. */
  private follow(): void {
    for (const s of this.stops) s();
    this.stops = [];
    const client = this.given ?? (this.ownClient ??= createClient({ db: this.getAttribute('db') || 'app', owner: true }));
    const had = studio.client;
    if (studio.use(client) && had) {
      // Another database: drop all Studio showed of the last one. The app root makes its pages
      // again, for the new client.
      resetCollections();
      queryClient.clear();
    }
    databaseState.follow(studio.client!);
    this.stops = [followEngine(studio.client!), () => databaseState.stop()];
    this.followed = client;
  }
}

if (!customElements.get('moul-studio')) customElements.define('moul-studio', MoulStudio);

declare global {
  interface HTMLElementTagNameMap {
    'moul-studio': MoulStudio;
  }
}
