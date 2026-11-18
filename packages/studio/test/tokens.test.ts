import css from '@moul-dev/web-component/style.css?raw';
import { describe, expect, it } from 'vitest';
import { scopeTokens } from '../src/lib/tokens';

describe('the token sheet Studio adds to a page', () => {
  it('leaves the page’s own color scheme and theme classes alone', () => {
    const out = scopeTokens(css);
    const rootBlock = out.slice(out.indexOf(':root'), out.indexOf('}', out.indexOf(':root')));
    expect(rootBlock).toContain('moul-studio');
    expect(rootBlock).not.toContain('color-scheme');
    expect(out).not.toMatch(/^\s*\.dark\s*\{/m);
    expect(out).not.toMatch(/^\[data-theme="dark"\],/m);
    expect(out).toContain('moul-studio[data-theme="dark"] {');
    expect(out).toContain('moul-studio[data-theme="light"] {');
  });
});
