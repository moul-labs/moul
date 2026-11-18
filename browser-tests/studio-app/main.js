// An app that embeds Studio, as the packages' READMEs show: both come by package name, so the
// bundler takes them through their `exports` (Studio's dist build, the SDK's dist and worker).
import '@moul-dev/studio';
import { createClient } from '@moul-dev/client';

const db = new URLSearchParams(location.search).get('db') || 'app';
const moul = createClient({ db, owner: true });
document.querySelector('moul-studio').client = moul;

window.moul = moul;
window.ready = true;
