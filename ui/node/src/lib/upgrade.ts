// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/upgrade/check`: the same status `fluxctl upgrade --check --json` prints.

import type { Tone } from './health';

export type UpgradeState = 'up_to_date' | 'update_available' | 'no_installable_release' | 'unreachable';

export interface SeenRelease {
  tag: string;
  installable: boolean;
  note: string;
}

export interface UpgradeStatus {
  schema: number;
  state: UpgradeState;
  current: string;
  latest: string | null;
  tag: string | null;
  update_available: boolean;
  source: string;
  looked_for: string;
  newest: SeenRelease[];
  line: string;
  maintainer_next: string | null;
  user_next: string;
}

export const UPGRADE_COMMAND = 'fluxctl upgrade --check';

export const upgradeTone = (s: Pick<UpgradeStatus, 'state'>): Tone => (s.state === 'update_available' ? 'warn' : 'ok');

export const upgradeWord = (s: Pick<UpgradeStatus, 'state'>): string =>
  ({ up_to_date: 'Up to date', update_available: 'Update available', no_installable_release: 'No release to install', unreachable: 'Not checked' })[s.state];

/** The lines under the status: what was looked for, the newest releases and why each was skipped, then the next steps. */
export const upgradeDetails = (s: UpgradeStatus): string[] => {
  if (s.state === 'up_to_date') return [];
  const out = [`Looked for: ${s.looked_for}.`];
  if (s.state === 'no_installable_release') {
    out.push(s.newest.length ? `Newest releases: ${s.newest.map((n) => (n.installable ? n.tag : `${n.tag} (${n.note})`)).join(', ')}.` : 'The server lists no releases.');
  }
  if (s.maintainer_next) out.push(`For the maintainer: ${s.maintainer_next}`);
  out.push(`For you: ${s.user_next}`);
  return out;
};
