import { Store } from './store';

export interface Route {
  /** Path relative to the console base, with `:name` segments, e.g. `/records/:moulName`. */
  path: string;
  /** Custom element rendered for this route. */
  tag?: string;
  /** Loads the module that defines `tag`; called once, on first visit. */
  load?: () => Promise<unknown>;
  /** `app` routes render inside `<admin-layout>`; `bare` routes fill the window. */
  layout?: 'app' | 'bare';
  /** Returns a path to redirect to instead of entering the route. */
  guard?: () => string | undefined;
  /** Unconditional redirect target. */
  redirect?: string;
}

export interface RouteLocation {
  /** Path relative to the console base; always starts with `/`. */
  pathname: string;
  search: URLSearchParams;
  params: Record<string, string>;
  route: Route | undefined;
}

export type SearchInit = Record<string, string | number | boolean | null | undefined> | URLSearchParams | string;

export interface NavigateOptions {
  search?: SearchInit;
  replace?: boolean;
}

function toSearchParams(init: SearchInit | undefined): URLSearchParams {
  if (init === undefined) return new URLSearchParams();
  if (typeof init === 'string' || init instanceof URLSearchParams) return new URLSearchParams(init);
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(init)) {
    if (value !== undefined && value !== null && value !== false) params.set(key, String(value));
  }
  return params;
}

function matchPath(pattern: string, pathname: string): Record<string, string> | undefined {
  const want = pattern.split('/').filter(Boolean);
  const have = pathname.split('/').filter(Boolean);
  if (want.length !== have.length) return undefined;
  const params: Record<string, string> = {};
  for (let i = 0; i < want.length; i++) {
    if (want[i].startsWith(':')) {
      params[want[i].slice(1)] = decodeURIComponent(have[i]);
    } else if (want[i] !== have[i]) {
      return undefined;
    }
  }
  return params;
}

/** Where the router keeps the current path. */
export type RoutingMode =
  /** Inside the router only: the page's URL is left alone. For Studio embedded in an app. */
  | 'memory'
  /** In the URL's fragment (`#/records/tasks`), so reloads and links keep the page. */
  | 'hash';

/**
 * Studio's router. Embedded in an app it must not take over the page's URL, so it keeps the path
 * in memory or in the URL's fragment. Paths passed to `navigate` and `href` start with `/`.
 * Clicks on Studio's own `<a href="#/…">` links, including anchors inside shadow roots, are routed
 * here; links elsewhere on the page are left to the page.
 */
export class Router extends Store {
  /** The table passed to `start()`; the dev inspector lists it. */
  routes: readonly Route[] = [];
  location: RouteLocation = { pathname: '/', search: new URLSearchParams(), params: {}, route: undefined };
  mode: RoutingMode = 'memory';
  private current = '/';
  private root: EventTarget | undefined;
  private started = false;

  /** Starts routing clicks inside `root`. */
  start(routes: Route[], options: { mode?: RoutingMode; root?: EventTarget; initial?: string } = {}): void {
    this.routes = routes;
    this.mode = options.mode ?? 'memory';
    this.root = options.root;
    this.current = options.initial ?? '/';
    if (!this.started) {
      this.started = true;
      window.addEventListener('hashchange', () => {
        if (this.mode === 'hash') this.resolve();
      });
      document.addEventListener('click', this.onClick);
    }
    this.resolve();
  }

  /** The `href` for a Studio path, for `<a href>` and `<moul-link href>`. */
  href(to: string, search?: SearchInit): string {
    const [path, query] = to.split('?');
    const params = toSearchParams(query);
    for (const [key, value] of toSearchParams(search)) params.set(key, value);
    const qs = params.toString();
    return `#${path.startsWith('/') ? path : `/${path}`}${qs ? `?${qs}` : ''}`;
  }

  /** The current path and query, as `navigate` takes them. */
  get path(): string {
    return this.mode === 'hash' ? window.location.hash.replace(/^#/, '') || '/' : this.current;
  }

  navigate(to: string, options: NavigateOptions = {}): void {
    const target = this.href(to, options.search).slice(1);
    if (target === this.path) return;
    if (this.mode === 'hash') {
      const url = `${window.location.pathname}${window.location.search}#${target}`;
      if (options.replace) window.history.replaceState(null, '', url);
      else window.history.pushState(null, '', url);
    } else {
      this.current = target;
    }
    this.resolve();
  }

  /** Re-runs route guards against the current path. */
  refresh(): void {
    this.resolve();
  }

  isActive(prefix: string): boolean {
    return this.location.pathname === prefix || this.location.pathname.startsWith(`${prefix}/`);
  }

  private replace(to: string): void {
    if (this.mode === 'hash') {
      window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}#${to}`);
    } else {
      this.current = to;
    }
  }

  private resolve(): void {
    // Redirects can chain (`/` → `/overview`); cap the hops.
    for (let hop = 0; hop < 5; hop++) {
      const [rawPath, search = ''] = this.path.split('?');
      const pathname = `/${rawPath.replace(/^\/+|\/+$/g, '')}`;

      let route: Route | undefined;
      let params: Record<string, string> = {};
      for (const candidate of this.routes) {
        const matched = matchPath(candidate.path, pathname);
        if (matched) {
          route = candidate;
          params = matched;
          break;
        }
      }

      const redirectTo = route?.redirect ?? route?.guard?.();
      if (redirectTo) {
        this.replace(this.href(redirectTo).slice(1));
        continue;
      }

      this.location = { pathname, search: new URLSearchParams(search), params, route };
      this.notify();
      return;
    }
  }

  private readonly onClick = (event: MouseEvent) => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return;
    }
    const path = event.composedPath();
    if (this.root && !path.includes(this.root)) return;
    const anchor = path.find((node): node is HTMLAnchorElement => node instanceof HTMLAnchorElement);
    if (!anchor || anchor.hasAttribute('download') || (anchor.target && anchor.target !== '_self')) return;
    const href = anchor.getAttribute('href') ?? '';
    if (!href.startsWith('#/')) return;

    event.preventDefault();
    this.navigate(href.slice(1));
  };
}

export const router = new Router();
