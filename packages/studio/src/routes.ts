import type { Route } from './lib/router';

/** Console page inside the shell. */
function page(path: string, tag: string, load: () => Promise<unknown>): Route {
  return { path, tag, load, layout: 'app' };
}

// First match wins. Pages load on first visit, so each one is its own chunk.
export const routes: Route[] = [
  { path: '/', redirect: '/overview' },
  page('/overview', 'admin-overview-page', () => import('./pages/overview')),
  page('/collections', 'admin-collections-page', () => import('./pages/collections')),
  page('/collections/:moulName', 'admin-collection-page', () => import('./pages/collection')),
  page('/records/:moulName', 'admin-records-page', () => import('./pages/records')),
  page('/realtime', 'admin-realtime-page', () => import('./pages/realtime')),
  page('/inbox', 'admin-inbox-page', () => import('./pages/inbox')),
  page('/database', 'admin-database-page', () => import('./pages/database')),
  page('/sync', 'admin-sync-page', () => import('./pages/sync')),
  page('/schema', 'admin-schema-page', () => import('./pages/schema')),
];
