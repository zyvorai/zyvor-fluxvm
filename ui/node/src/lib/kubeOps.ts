// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shapes and words of the Studio's cluster operations card: `GET /v1/k3s/{cluster}/upgrade/plan`, `POST /v1/k3s/{cluster}/upgrade`,
// `POST /v1/k3s/{cluster}/nodes/add`, `POST /v1/k3s/{cluster}/nodes/{node}/remove` and `GET /v1/k3s-ops/{id}`. The logic is `fluxctl kube upgrade`
// and `fluxctl kube node`; this file only words what they return.

import type { Tone } from './health';

export type OpKind = 'upgrade' | 'node-add' | 'node-remove';
export type OpState = 'running' | 'done' | 'failed';

export interface KubeOp {
  id: string;
  cluster: string;
  kind: OpKind;
  state: OpState;
  /** The terminal command that does the same. */
  command: string;
  lines: string[];
  error: string | null;
  explain: string | null;
  started_secs_ago: number;
}

export interface UpgradePlan {
  cluster: string;
  channel: string;
  plan: string;
  nothing_to_do: boolean;
  command: string;
}

export const upgradePlanPath = (cluster: string, channel: string): string =>
  `/v1/k3s/${encodeURIComponent(cluster)}/upgrade/plan?channel=${encodeURIComponent(channel)}`;
export const upgradePath = (cluster: string): string => `/v1/k3s/${encodeURIComponent(cluster)}/upgrade`;
export const nodeAddPath = (cluster: string): string => `/v1/k3s/${encodeURIComponent(cluster)}/nodes/add`;
export const nodeRemovePath = (cluster: string, node: string): string =>
  `/v1/k3s/${encodeURIComponent(cluster)}/nodes/${encodeURIComponent(node)}/remove`;
export const opPath = (id: string): string => `/v1/k3s-ops/${encodeURIComponent(id)}`;

const KIND_WORD: Record<OpKind, string> = { upgrade: 'Upgrade', 'node-add': 'Adding a node', 'node-remove': 'Removing a node' };
export const kindWord = (k: OpKind): string => KIND_WORD[k] ?? k;

export const opTone = (s: OpState): Tone => (s === 'done' ? 'ok' : s === 'failed' ? 'bad' : 'warn');
export const opWord = (o: Pick<KubeOp, 'kind' | 'state'>): string =>
  o.state === 'running' ? `${kindWord(o.kind)} is running` : o.state === 'done' ? `${kindWord(o.kind)} finished` : `${kindWord(o.kind)} failed`;

/** The fluxctl command that explains a failure, when the failure carries a code. */
export const explainCommand = (o: Pick<KubeOp, 'explain'>): string | null => (o.explain ? `fluxctl explain ${o.explain}` : null);

/** The failure without the trailing `more: fluxctl explain CODE` line (the card shows that as a copyable command). */
export function failureText(o: Pick<KubeOp, 'error'>): string {
  return (o.error ?? '')
    .split('\n')
    .filter((l) => !/^\s*more:\s*fluxctl explain /.test(l))
    .join('\n')
    .trim();
}

/** What to tell the person to do next, in words, per kind of failure; the codes carry the full list (`fluxctl explain`). */
export function nextStep(o: Pick<KubeOp, 'kind' | 'state' | 'explain'>): string | null {
  if (o.state !== 'failed') return null;
  if (o.explain === 'KUBE_UPGRADE_REFUSED') return 'Nothing was changed. Fix what the message names (an unhealthy node, a skipped version), then look at the plan again.';
  if (o.explain === 'KUBE_OP_BUSY') return 'Wait for the running operation to finish, then try again.';
  if (o.explain === 'KUBE_NODE_UNSUPPORTED') return 'This cluster cannot do that from here. Use the steps in the message in a terminal.';
  if (o.kind === 'upgrade') return 'The pre-upgrade backup is kept. Check the cluster with fluxctl cluster status before trying again; a restore from that backup is on the Backups card.';
  return 'Check the cluster with fluxctl cluster status, then try again.';
}

/** The two-step confirmation: the action only works once the cluster's (or node's) name is typed again. */
export const confirmed = (typed: string, name: string): boolean => typed.trim() === name;

/** What an upgrade does, in a sentence, shown above the plan. */
export const UPGRADE_WARNING =
  'A verified backup is taken first. Then k3s is restarted on the server (the API is gone for about a minute; running pods keep running) and on each worker in turn.';

export const NODE_ADD_WARNING = 'Starts one more worker VM on this machine and joins it to the cluster. Nothing existing is changed.';

export const NODE_REMOVE_WARNING =
  'Removing a node is not automatic yet. Studio shows the steps: drain the node, delete it from the cluster, then stop its machine.';
