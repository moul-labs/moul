import { until } from 'lit/directives/until.js';
import { getFileUrl } from '../components/records/record-helpers';
import { moul } from '../studio';

/**
 * A file's URL for `<img src>` and `<a href>`. The engine runs in the page, so its `/storage/…`
 * paths do not resolve; the SDK hands out blob URLs instead.
 */
export function fileSrc(value: unknown) {
  const url = getFileUrl(value);
  if (!url) return '';
  return until(
    moul()
      .files.url(url)
      .catch(() => ''),
    '',
  );
}

/** Opens a file in a new tab (images, PDFs, text) or downloads it (anything else). */
export async function openFile(value: unknown): Promise<void> {
  const url = getFileUrl(value);
  if (!url) return;
  window.open(await moul().files.url(url), '_blank', 'noopener');
}
