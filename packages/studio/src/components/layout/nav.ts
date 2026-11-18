import type { UserInfo } from '../../state/auth';

export interface NavLink {
  to: string;
  label: string;
  icon: string;
  /** Shown on the mobile "More" drawer cards. */
  description?: string;
}

export const platformNavLinks: NavLink[] = [
  { to: '/overview', label: 'Overview', icon: 'squares-four' },
  { to: '/collections', label: 'Collections', icon: 'database' },
  { to: '/realtime', label: 'Realtime', icon: 'broadcast' },
  { to: '/inbox', label: 'Inbox', icon: 'envelope-simple', description: 'Sign-in codes and reset links' },
];

export const systemNavLinks: NavLink[] = [
  { to: '/database', label: 'Database', icon: 'hard-drives', description: 'The file, backups and storage' },
  { to: '/sync', label: 'Sync', icon: 'arrows-clockwise', description: 'Your other devices, through a bucket' },
  { to: '/schema', label: 'Schema & types', icon: 'code', description: 'For git, agents and TypeScript' },
];

export const allNavLinks = [...platformNavLinks, ...systemNavLinks];

export interface Identity {
  displayName: string;
  displayUsername: string;
  displayEmail: string;
  hasCustomName: boolean;
  /** `@user • email` or `email (@user)`, as shown under the name in account menus. */
  subtitle: string;
}

export function identityOf(user: UserInfo | null | undefined): Identity {
  const hasCustomName = Boolean(
    user?.name &&
      user.name.trim() !== '' &&
      user.name.trim().toLowerCase() !== user.username?.trim().toLowerCase(),
  );
  const displayUsername = user?.username || 'admin';
  const displayName = hasCustomName ? user!.name!.trim() : displayUsername;
  const displayEmail = user?.email || '';
  const subtitle = hasCustomName
    ? `@${displayUsername}${displayEmail ? ` • ${displayEmail}` : ''}`
    : displayEmail
      ? `${displayEmail} (@${displayUsername})`
      : `@${displayUsername}`;
  return { displayName, displayUsername, displayEmail, hasCustomName, subtitle };
}
