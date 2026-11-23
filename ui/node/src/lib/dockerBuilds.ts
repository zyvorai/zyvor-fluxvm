// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Builds card (Images > Builds): the `docker build` history of the daemon's built-in Docker socket.
import type { Tone } from './kube';

export type DockerBuild = {
  id: string;
  started_unix: number;
  finished_unix: number | null;
  duration_ms: number | null;
  /** `running`, `succeeded`, `failed` or `cancelled`. */
  state: string;
  tags: string[];
  dockerfile: string;
  /** Names only; values are never kept. */
  build_arg_names: string[];
  target: string | null;
  platform: string | null;
  no_cache: boolean;
  context_bytes: number;
  image_id: string | null;
  error: string | null;
  builder: string;
  /** The `fluxctl` command that does the same build. */
  fluxctl: string;
};

export type DockerBuildsView = {
  enabled: boolean;
  available: boolean;
  /** Why it cannot build, and what to do. */
  reason: string | null;
  builder?: string;
  socket?: string | null;
  builds: DockerBuild[];
  try: string;
  fluxctl: string;
  /** The command that points `docker` at the socket. */
  context?: string;
};

export const CONTEXT_COMMAND = 'fluxctl docker context install --builtin --use';
export const HISTORY_COMMAND = 'fluxctl docker builds';

export const buildTone = (state: string): Tone => {
  switch (state) {
    case 'succeeded':
      return 'ok';
    case 'running':
      return 'info';
    case 'failed':
      return 'bad';
    default:
      return 'idle';
  }
};

export const buildLabel = (state: string): string => (state === 'succeeded' ? 'built' : state === 'cancelled' ? 'stopped' : state);

/** `75s` -> `1m 15s`; nothing yet -> an em dash. */
export const durationText = (ms: number | null): string => {
  if (ms === null || ms === undefined) return '—';
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
};

/** Docker's short image id: 12 characters without the `sha256:`. */
export const shortImage = (id: string | null): string => (id ? id.replace(/^sha256:/, '').slice(0, 12) : '—');

export const tagsText = (b: Pick<DockerBuild, 'tags'>): string => (b.tags.length ? b.tags.join(', ') : '(untagged)');

export const summarizeBuilds = (bs: DockerBuild[]): { built: number; failed: number; running: number } => ({
  built: bs.filter((b) => b.state === 'succeeded').length,
  failed: bs.filter((b) => b.state === 'failed').length,
  running: bs.filter((b) => b.state === 'running').length,
});

/** The sentence under the card title: what works now, or why it cannot and what to run. */
export const builderNote = (v: DockerBuildsView): string => {
  if (!v.enabled) return v.reason ?? 'The built-in Docker socket is off on this host.';
  if (!v.available) return v.reason ?? 'This host cannot build.';
  return `docker build on the FluxVM socket is forwarded to ${v.builder ?? 'the Mac builder'}.`;
};

/** The last line of a failure, which is the one that says what to do. */
export const errorLine = (b: Pick<DockerBuild, 'error'>): string => {
  const lines = (b.error ?? '').split('\n').filter((l) => l.trim());
  return lines.length ? lines[lines.length - 1] : '';
};

export const sizeText = (n: number): string => (n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(1)} GiB` : n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MiB` : `${Math.max(1, Math.round(n / 1024))} KiB`);
