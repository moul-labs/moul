/**
 * Moul UI's token sheet, made safe to add to someone else's page: the tokens stay on `:root` (and
 * on `<moul-studio>`), but the page's own `color-scheme` is left alone, and the theme rules that
 * would match the page's own `.dark` or `[data-theme]` elements apply to `<moul-studio>` only.
 */
export function scopeTokens(css: string): string {
  return css
    .replace(/:root\s*\{([^}]*?)\s*color-scheme:\s*light dark;/, ':root {$1')
    .replace(/:root\s*\{/, ':root,\nmoul-studio {')
    .replace(/\[data-theme="light"\],\s*\.light\s*\{/, 'moul-studio[data-theme="light"] {')
    .replace(/\[data-theme="dark"\],\s*\.dark\s*\{/, 'moul-studio[data-theme="dark"] {');
}
