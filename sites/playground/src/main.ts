import '@moul-dev/web-component/style.css';
import './playground';

// The service worker keeps the page working offline once it has loaded. Not in development,
// where it would serve stale modules.
if ('serviceWorker' in navigator && import.meta.env.PROD) {
  void navigator.serviceWorker.register('/sw.js', { type: 'module' }).catch(() => {});
}

// Studio loads each page's code when it is first opened. After a deploy, a tab opened before it can
// find that code gone from the server when the service worker has not kept it: load the page again,
// once, to get the current version. If that fails again within a minute, the error shows instead.
const RELOADED_KEY = 'moul.playground.reloaded';
window.addEventListener('vite:preloadError', (event) => {
  try {
    const last = Number(sessionStorage.getItem(RELOADED_KEY));
    if (last && Date.now() - last < 60_000) return;
    sessionStorage.setItem(RELOADED_KEY, String(Date.now()));
  } catch {
    // Without a note of it, a reload could repeat.
    return;
  }
  event.preventDefault();
  window.location.reload();
});
