import type { NavGroup } from '@fluxvm/ui-core';
import type { Role } from './lib/fleet';

export const navFor = (role: Role): NavGroup[] => [
  { label: 'Overview', items: [{ to: '/', label: 'Overview' }] },
  {
    label: 'Compute',
    items: [
      { to: '/hosts', label: role === 'operator' ? 'Hosts' : 'My hosts', desc: role === 'operator' ? 'Every machine that reports in' : 'Hosts you lease' },
      { to: '/vms', label: 'Virtual machines', desc: 'Launch and control VMs across hosts' },
      { to: '/networks', label: 'Networks', desc: 'Private networks across hosts' },
    ],
  },
  {
    label: 'Account',
    items: [
      ...(role === 'operator' ? [{ to: '/tenants', label: 'Tenants', desc: 'Quotas, tokens and keys' }] : [{ to: '/keys', label: 'API keys', desc: 'Named keys for your tools' }]),
      { to: '/billing', label: 'Billing & usage', desc: 'What it costs, and why' },
    ],
  },
  { label: 'Activity', items: [{ to: '/activity', label: 'Activity' }] },
];
