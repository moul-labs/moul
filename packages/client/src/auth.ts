import type { MoulClient } from './client.js';
import { MoulError } from './errors.js';
import { report } from './live.js';
import type { RecordModel } from './types.js';

/** Where a sign-in is kept: `localStorage` by default in a browser. */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** Whether two sessions are the same person (a token refresh), not a different sign-in. */
export function samePerson(a: AuthSession | null, b: AuthSession | null): boolean {
  if (!a || !b) return a === b;
  return a.collection === b.collection && a.record?.id === b.record?.id;
}

/** A signed-in person: their token, their record, and the auth collection they signed in to. */
export interface AuthSession<T = RecordModel> {
  token: string;
  record: T;
  collection: string;
}

/**
 * The sign-in a client sends with its requests. It is kept in `localStorage` (when there is one),
 * so it survives reloads and is shared by the site's tabs: signing in or out in one tab does the
 * same in the others.
 */
export class AuthStore {
  private value: AuthSession | null = null;
  private readonly listeners = new Set<(session: AuthSession | null, previous: AuthSession | null) => void>();
  private unlisten: (() => void) | undefined;

  constructor(
    private readonly storage: StorageLike | null,
    private readonly key: string,
  ) {
    this.value = this.load();
    const w = globalThis as any;
    if (storage && typeof w.addEventListener === 'function' && storage === w.localStorage) {
      const onStorage = (e: StorageEvent) => {
        if (e.key !== key) return;
        const previous = this.value;
        this.value = this.load();
        this.emit(previous);
      };
      w.addEventListener('storage', onStorage);
      this.unlisten = () => w.removeEventListener('storage', onStorage);
    }
  }

  /** Stops following other tabs' sign-ins. */
  close(): void {
    this.unlisten?.();
    this.unlisten = undefined;
    this.listeners.clear();
  }

  private load(): AuthSession | null {
    try {
      const raw = this.storage?.getItem(this.key);
      if (!raw) return null;
      const s = JSON.parse(raw);
      return typeof s?.token === 'string' && s.record && typeof s.collection === 'string' ? s : null;
    } catch {
      return null;
    }
  }

  /** Tells the listeners; one that throws is reported and stops neither the others nor the caller. */
  private emit(previous: AuthSession | null): void {
    for (const l of [...this.listeners]) {
      try {
        l(this.value, previous);
      } catch (err) {
        report(err);
      }
    }
  }

  /** The session, or `null` when nobody is signed in. */
  get session(): AuthSession | null {
    return this.value;
  }

  /** The token, or `null`. */
  get token(): string | null {
    return this.value?.token ?? null;
  }

  /** The signed-in record, or `null`. */
  get record(): RecordModel | null {
    return this.value?.record ?? null;
  }

  /** Replaces the session; `null` signs out. */
  set(session: AuthSession | null): void {
    const previous = this.value;
    this.value = session;
    try {
      if (session) this.storage?.setItem(this.key, JSON.stringify(session));
      else this.storage?.removeItem(this.key);
    } catch {
      // Storage may be full or blocked; the session still holds in memory.
    }
    this.emit(previous);
  }

  /**
   * Calls `listener` when the session changes, here or in another tab, with the session before.
   * A token refresh for the same person changes the session too; {@link samePerson} tells them
   * apart.
   */
  onChange(listener: (session: AuthSession | null, previous: AuthSession | null) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}

/** Sign-in for one auth collection: `moul.auth('users')`. */
export class AuthClient<T extends RecordModel = RecordModel> {
  constructor(
    private readonly client: MoulClient<any>,
    /** The auth collection's name. */
    readonly collection: string,
  ) {}

  private path(action: string): string {
    return `/api/moul/${encodeURIComponent(this.collection)}/${action}`;
  }

  private keep(answer: { token: string; record: T }): AuthSession<T> {
    const session = { token: answer.token, record: answer.record, collection: this.collection };
    this.client.authStore.set(session);
    return session;
  }

  /**
   * Creates an account: a record in the auth collection. It does not sign in. Without a
   * `username`, one is made from the email address, as signing in with a code does.
   */
  async signUp(data: { email: string; password: string; passwordConfirm?: string; username?: string; [field: string]: unknown }): Promise<T> {
    const records = this.client.collection<T>(this.collection);
    const record = { passwordConfirm: data.password, ...data } as Record<string, unknown>;
    if (data.username) return records.create(record as any);
    const base = data.email.split('@')[0].replace(/[^A-Za-z0-9_]/g, '') || 'user';
    try {
      return await records.create({ ...record, username: base } as any);
    } catch (e) {
      // The name may be taken: add a few characters, as the engine does for code sign-ups.
      if (!(e instanceof MoulError) || e.status !== 400 || !/username/i.test(JSON.stringify(e.data ?? e.message))) throw e;
      const suffix = Math.random().toString(36).slice(2, 6);
      return records.create({ ...record, username: `${base}_${suffix}` } as any);
    }
  }

  /** Signs in with an email or username and a password. */
  async signInWithPassword(identity: string, password: string): Promise<AuthSession<T>> {
    const answer = await this.client.request<{ token: string; record: T }>({
      method: 'POST',
      path: this.path('auth-with-password'),
      json: { identity, password },
      anonymous: true,
    });
    return this.keep(answer);
  }

  /**
   * Sends a sign-in code to `email`, making an account if there is none. In a tab the engine cannot
   * send mail: the code lands in the local inbox, which Studio shows (`moul.admin.inbox()`).
   */
  async requestCode(email: string): Promise<void> {
    await this.client.request({ method: 'POST', path: this.path('otp/request'), json: { email }, anonymous: true });
  }

  /** Signs in with the code sent to `email`. */
  async signInWithCode(email: string, code: string): Promise<AuthSession<T>> {
    const answer = await this.client.request<{ token: string; record: T }>({
      method: 'POST',
      path: this.path('auth-with-otp'),
      json: { email, code },
      anonymous: true,
    });
    return this.keep(answer);
  }

  /**
   * Exchanges the current token for a new one, and reloads the signed-in record. If the engine
   * refuses (the token was revoked or expired too long ago), the session is cleared, so the client
   * does not go on sending a token that no longer works. If the person signed out or someone else
   * signed in while the refresh was out, its answer is dropped (and its new token revoked).
   */
  async refresh(): Promise<AuthSession<T>> {
    const sent = this.client.authStore.token;
    if (!sent) throw new MoulError(401, 'nobody is signed in');
    const current = () => this.client.authStore.token === sent;
    try {
      const answer = await this.client.request<{ token: string; record: T }>({
        method: 'POST',
        path: this.path('refresh'),
        // Sent even by an owner client, which leaves its sign-in out of other requests.
        headers: { Authorization: `Bearer ${sent}` },
        anonymous: true,
      });
      if (!current()) {
        void this.revoke(answer.token);
        throw new MoulError(409, 'the sign-in changed while the token was being refreshed');
      }
      return this.keep(answer);
    } catch (e) {
      if (e instanceof MoulError && e.status === 401 && current()) this.client.authStore.set(null);
      throw e;
    }
  }

  private async revoke(token: string): Promise<void> {
    try {
      await this.client.request({
        method: 'POST',
        path: this.path('logout'),
        headers: { Authorization: `Bearer ${token}` },
        anonymous: true,
      });
    } catch {
      // Best effort: the session is gone here either way.
    }
  }

  /** Signs out: revokes the token and forgets the session. */
  async signOut(): Promise<void> {
    const token = this.client.authStore.token;
    try {
      this.client.authStore.set(null);
    } finally {
      if (token) await this.revoke(token);
    }
  }

  /** Sends a password reset link to `email` (in a tab: to the local inbox). */
  async requestPasswordReset(email: string): Promise<void> {
    await this.client.request({ method: 'POST', path: this.path('request-password-reset'), json: { email }, anonymous: true });
  }

  /** Sets a new password with the token from a reset link. */
  async confirmPasswordReset(token: string, password: string, passwordConfirm = password): Promise<void> {
    await this.client.request({
      method: 'POST',
      path: this.path('confirm-password-reset'),
      json: { token, password, passwordConfirm },
      anonymous: true,
    });
  }

  /** Which sign-in methods the collection offers. */
  methods(): Promise<Record<string, { enabled: boolean; [key: string]: unknown }>> {
    return this.client.request({ path: this.path('auth-methods'), anonymous: true });
  }
}
