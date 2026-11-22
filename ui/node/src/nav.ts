// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

import type { NavGroup } from '@fluxvm/ui-core';

// Phases add groups here as their pages land; nothing is listed that does not work.
export const NAV: NavGroup[] = [
  {
    label: 'Home',
    items: [
      { to: '/', label: 'Home', desc: 'Is everything OK, in plain words' },
      { to: '/overview', label: 'Overview', desc: 'Machine counts and live events' },
    ],
  },
  {
    label: 'Machines',
    items: [
      { to: '/machines', label: 'Machines' },
      { to: '/branches', label: 'Branches', desc: 'Saved machines to start again' },
    ],
  },
  {
    label: 'Agents',
    items: [
      { to: '/sandboxes', label: 'Sandboxes', desc: 'Disposable, isolated machines' },
      { to: '/agents', label: 'Agent sessions', desc: 'Confined workspaces for agents' },
    ],
  },
  {
    label: 'Platform',
    items: [
      { to: '/network', label: 'Network', desc: 'Outbound policy and live traffic' },
      { to: '/kubernetes', label: 'Kubernetes', desc: 'Clusters, nodes and pods' },
      { to: '/docker', label: 'Docker', desc: 'Containers and images of the engine' },
      { to: '/images', label: 'Images', desc: 'Catalog, templates and warm pools' },
      { to: '/volumes', label: 'Volumes', desc: 'Disks that outlive a machine' },
      { to: '/backups', label: 'Backups', desc: 'Standalone disk copies' },
      { to: '/host', label: 'Host', desc: 'Memory, GPUs and topology' },
      { to: '/access', label: 'Access & audit', desc: 'Quota, events and the API' },
    ],
  },
];
