import { Store } from '../lib/store';

export type ThemeMode = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

const STORAGE_KEY = 'moul_theme_mode';
const CYCLE: Record<ThemeMode, ThemeMode> = { system: 'light', light: 'dark', dark: 'system' };

function getStoredTheme(): ThemeMode {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === 'light' || saved === 'dark' || saved === 'system') {
      return saved;
    }
  } catch {
    // localStorage might be inaccessible (e.g. iframe or strict privacy mode)
  }
  return 'system';
}

/**
 * Theme mode (system, light, dark). The `@moul-dev/web-component` tokens use `light-dark()`, so the
 * theme is applied by setting `color-scheme` and `data-theme` on each `<moul-studio>`, never on the
 * page around it.
 */
class ThemeStore extends Store {
  theme: ThemeMode = getStoredTheme();
  private readonly systemQuery = window.matchMedia('(prefers-color-scheme: dark)');
  private readonly hosts = new Set<HTMLElement>();

  constructor() {
    super();
    this.systemQuery.addEventListener('change', () => {
      this.apply();
      this.notify();
    });
  }

  get resolvedTheme(): ResolvedTheme {
    if (this.theme === 'system') return this.systemQuery.matches ? 'dark' : 'light';
    return this.theme;
  }

  setTheme(theme: ThemeMode): void {
    this.theme = theme;
    try {
      localStorage.setItem(STORAGE_KEY, theme);
    } catch {
      // ignore storage errors
    }
    this.apply();
    this.notify();
  }

  /** Cycles system → light → dark → system. */
  cycleTheme(): void {
    this.setTheme(CYCLE[this.theme]);
  }

  /** Themes `host`, a `<moul-studio>`, until the returned function is called. */
  attach(host: HTMLElement): () => void {
    this.hosts.add(host);
    this.applyTo(host);
    return () => this.hosts.delete(host);
  }

  private apply(): void {
    for (const host of this.hosts) this.applyTo(host);
  }

  private applyTo(host: HTMLElement): void {
    host.setAttribute('data-theme', this.resolvedTheme);
    host.style.colorScheme = this.theme === 'system' ? 'light dark' : this.theme;
  }
}

export const theme = new ThemeStore();
