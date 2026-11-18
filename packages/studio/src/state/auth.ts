import { Store } from '../lib/store';

export interface UserInfo {
  id?: string;
  username?: string;
  name?: string;
  email?: string;
  role?: string;
}

/**
 * Who uses Studio. In a tab the person at the keyboard owns the database, so there is no sign-in
 * to the console: Studio always works as the owner.
 */
class AuthStore extends Store {
  readonly isLoading = false;
  readonly isAuthenticated = true;
  readonly user: UserInfo = { username: 'owner', name: 'Owner', role: 'Owner' };

  async init(): Promise<void> {}
}

export const auth = new AuthStore();
