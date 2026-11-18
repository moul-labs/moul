// The Playground's service worker: once the page has loaded, it works offline. Every built file is
// cached when the worker installs (the list is written at build time). Built files have content
// hashes in their names, so they are served from the cache first; the page itself is fetched fresh
// when there is a network, and from the cache when there is not.
//
// A new version waits until no tab uses the one before (there is no skipWaiting): a tab opened
// before a deploy loads its pages' code lazily, by name, so the old version's files stay cached
// until the new worker takes over, and only then are they deleted.
const ASSETS = __ASSETS__;
const CACHE = "moul-playground-__VERSION__";

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(['/', '/icon.svg', '/manifest.webmanifest', ...ASSETS])));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

/** Whether `res` is this site's page as it served it: not an error, a redirect, or a file opened in the tab. */
function isPage(res) {
  return res.ok && res.type === 'basic' && (res.headers.get('content-type') ?? '').startsWith('text/html');
}

self.addEventListener('fetch', (event) => {
  const req = event.request;
  const url = new URL(req.url);
  // Only this site's own files. Requests to a sync bucket go straight to the network.
  if (req.method !== 'GET' || url.origin !== self.location.origin) return;
  if (req.mode === 'navigate') {
    event.respondWith(
      fetch(req)
        .then((res) => {
          if (isPage(res)) {
            const copy = res.clone();
            void caches.open(CACHE).then((c) => c.put('/', copy));
          }
          return res;
        })
        .catch(() => caches.open(CACHE).then((c) => c.match('/')).then((hit) => hit ?? Response.error())),
    );
    return;
  }
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(
      caches.match(req).then(
        (hit) =>
          hit ||
          fetch(req).then((res) => {
            if (res.ok && res.type === 'basic') {
              const copy = res.clone();
              void caches.open(CACHE).then((c) => c.put(req, copy));
            }
            return res;
          }),
      ),
    );
  }
});
