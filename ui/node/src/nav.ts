import type { NavGroup } from '@fluxvm/ui-core';

// Phases add groups here as their pages land; nothing is listed that does not work.
export const NAV: NavGroup[] = [
  { label: 'Overview', items: [{ to: '/', label: 'Overview' }] },
  { label: 'Machines', items: [{ to: '/machines', label: 'Machines' }] },
  {
    label: 'Platform',
    items: [
      { to: '/network', label: 'Network', desc: 'Outbound policy and live traffic' },
      { to: '/backups', label: 'Backups', desc: 'Standalone disk copies' },
    ],
  },
];
