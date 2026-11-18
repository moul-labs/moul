// Development page: Studio on a database in this browser, routed in the URL's fragment.
import './element';

// The Moul Inspector is a development aid; `vite build` drops this branch and its chunk.
if (import.meta.env.DEV) void import('./devtools/inspector');
