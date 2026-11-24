// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

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
      { to: '/mac', label: 'Mac', desc: 'macOS instances of fluxctl mac' },
      { to: '/apps', label: 'Apps', desc: 'Run an image on a cluster' },
      { to: '/docker', label: 'Docker', desc: 'Containers and images of the engine' },
      { to: '/mac-fleet', label: 'Mac fleet', desc: 'Capacity, thermal and disk of each Mac' },
      { to: '/templates', label: 'Templates', desc: 'One-command stacks' },
      { to: '/images', label: 'Images', desc: 'Catalog, templates and warm pools' },
      { to: '/volumes', label: 'Volumes', desc: 'Disks that outlive a machine' },
      { to: '/backups', label: 'Backups', desc: 'Standalone disk copies' },
      { to: '/usage', label: 'Usage', desc: 'CPU, memory and cost per machine' },
      { to: '/host', label: 'Host', desc: 'Memory, GPUs and topology' },
      { to: '/access', label: 'Access & audit', desc: 'Quota, events and the API' },
      { to: '/audit', label: 'Audit log', desc: 'Who changed what, and when' },
      { to: '/events', label: 'Events', desc: 'Everything that happened, live' },
      { to: '/cheatsheet', label: 'Cheatsheet', desc: 'The commands you will want' },
    ],
  },
];
