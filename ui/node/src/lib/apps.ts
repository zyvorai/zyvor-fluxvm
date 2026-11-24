// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Apps page: the apps `fluxctl kube run` makes, and the fluxctl command for everything the page does.
import { type KubePod, type Tone } from './kube';
import { labelsMatch, parseQuery } from './labelFilter';

export type AppState = 'running' | 'starting' | 'stopped' | 'problem';

export type App = {
  namespace: string;
  name: string;
  image: string;
  desired: number;
  ready: number;
  port: number | null;
  state: AppState;
  /** One sentence: `2 of 2 running`, or what is wrong. */
  summary: string;
  /** The explain code for a problem (`fluxctl explain CODE`). */
  code: string | null;
  /** The command to run next for a problem. */
  next: string | null;
  pods: KubePod[];
  created: string | null;
  /** The labels you gave it with `kube run --label`; absent on an older daemon. */
  labels?: Record<string, string>;
};

export type Apps = { schema?: number; cluster?: string; apps: App[]; error?: { message: string; next: string } };

export const MAX_COPIES = 100;

export const appTone = (s: AppState): Tone => (s === 'running' ? 'ok' : s === 'starting' ? 'warn' : s === 'problem' ? 'bad' : 'idle');

export const appsPath = (cluster: string): string => `/v1/kube/${encodeURIComponent(cluster)}/apps`;
export const deleteAppPath = (cluster: string, a: Pick<App, 'namespace' | 'name'>): string => `${appsPath(cluster)}/${encodeURIComponent(a.namespace)}/${encodeURIComponent(a.name)}/delete`;

/** The same path the Kubernetes page scales with. */
export const scalePath = (cluster: string, a: Pick<App, 'namespace' | 'name'>): string => `/v1/kube/${encodeURIComponent(cluster)}/workloads/${encodeURIComponent(a.namespace)}/deployments/${encodeURIComponent(a.name)}/scale`;

export const clampCopies = (n: number): number => (Number.isFinite(n) ? Math.min(MAX_COPIES, Math.max(0, Math.round(n))) : 0);

const nsFlag = (a: Pick<App, 'namespace'>): string => (a.namespace === 'default' ? '' : ` -n ${a.namespace}`);

/** The fluxctl command for each thing the page does, with the cluster and namespace spelled out. */
export const appCommands = (cluster: string, a: Pick<App, 'namespace' | 'name'>) => {
  const c = ` --cluster ${cluster}`;
  const ns = nsFlag(a);
  return {
    logs: `fluxctl kube logs ${a.name} -f --name ${cluster}${ns}`,
    shell: `fluxctl kube shell ${a.name}${ns}${c}`,
    forward: `fluxctl kube forward ${a.name} --name ${cluster}${ns}`,
    expose: `fluxctl kube expose ${a.name}${ns}${c}`,
    scale: (n: number) => `fluxctl kube scale ${a.name} ${clampCopies(n)}${ns}${c}`,
    rollbackCmd: `fluxctl kube rollout undo ${a.name}${ns}${c}`,
    rm: `fluxctl kube rm ${a.name}${ns}${c}`,
  };
};

export type RunForm = { image: string; name: string; port: string; replicas: string; namespace: string };

export const emptyRunForm: RunForm = { image: '', name: '', port: '', replicas: '1', namespace: 'default' };

const NAME = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/** The sentence to show for a form that cannot be sent, or null when it can. The daemon checks again. */
export const runProblem = (f: RunForm): string | null => {
  const image = f.image.trim();
  if (!image) return 'Type an image to run, for example nginx.';
  if (/\s/.test(image) || image.startsWith('-')) return 'An image name has no spaces, like nginx or ghcr.io/you/app:1.0.';
  if (f.name.trim() && !NAME.test(f.name.trim())) return 'The name uses lowercase letters, digits and dashes, for example web.';
  if (f.namespace.trim() && !NAME.test(f.namespace.trim())) return 'The namespace uses lowercase letters, digits and dashes.';
  if (f.port.trim() && !(Number.isInteger(Number(f.port)) && Number(f.port) >= 1 && Number(f.port) <= 65535)) return 'The port is a number from 1 to 65535.';
  const n = Number(f.replicas || '1');
  if (!Number.isInteger(n) || n < 0 || n > MAX_COPIES) return `Copies is a number from 0 to ${MAX_COPIES}.`;
  return null;
};

/** The request body for `POST /v1/kube/{cluster}/apps`; empty fields are left out so the daemon picks the defaults. */
export const runBody = (f: RunForm): Record<string, unknown> => {
  const body: Record<string, unknown> = { image: f.image.trim() };
  if (f.name.trim()) body.name = f.name.trim();
  if (f.port.trim()) body.port = Number(f.port);
  if (f.replicas.trim() && Number(f.replicas) !== 1) body.replicas = Number(f.replicas);
  if (f.namespace.trim() && f.namespace.trim() !== 'default') body.namespace = f.namespace.trim();
  return body;
};

/** `fluxctl kube run IMAGE ...`: the command that does what the form does. */
export const runCommand = (cluster: string, f: RunForm): string => {
  let s = `fluxctl kube run ${f.image.trim() || 'IMAGE'}`;
  if (f.name.trim()) s += ` --name ${f.name.trim()}`;
  if (f.port.trim()) s += ` --port ${f.port.trim()}`;
  if (f.replicas.trim() && Number(f.replicas) !== 1) s += ` --replicas ${f.replicas.trim()}`;
  if (f.namespace.trim() && f.namespace.trim() !== 'default') s += ` -n ${f.namespace.trim()}`;
  return `${s} --cluster ${cluster}`;
};

/** A one-line count for the page: `3 apps, 1 with a problem`. */
/** Apps whose name, image, namespace or state contain every word, and that carry every `key=value` label term. */
export const filterApps = (apps: App[], q: string): App[] => {
  const { words, labels } = parseQuery(q);
  return apps.filter((a) => labelsMatch(a.labels, labels) && words.every((w) => `${a.name} ${a.image} ${a.namespace} ${a.state}`.toLowerCase().includes(w)));
};

export const appsSummary = (apps: App[]): string => {
  if (!apps.length) return 'No apps yet';
  const bad = apps.filter((a) => a.state === 'problem').length;
  return `${apps.length} app${apps.length === 1 ? '' : 's'}${bad ? `, ${bad} with a problem` : ''}`;
};
