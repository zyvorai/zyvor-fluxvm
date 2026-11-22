// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Docker page: how the daemon's /v1/docker answer is summarised, filtered and written out.
import { type Tone, ageOf } from './kube';

export type DockerPort = { private: number | null; public: number | null; type: string; ip: string | null };

export type DockerContainer = {
  id: string;
  name: string;
  image: string;
  command: string;
  /** `running`, `exited`, `created`, `paused`, `restarting`, `dead`. */
  state: string;
  /** Docker's own words: `Up 2 minutes`, `Exited (1) 3 hours ago`. */
  status: string;
  /** Unix seconds. */
  created: number;
  ports: DockerPort[];
};

export type DockerImage = { id: string; tags: string[]; size: number; created: number; containers: number };

export type DockerEngine = {
  vm: { id: string; name: string; status: string } | null;
  socket: string;
  reachable: boolean;
  /** Why it is not reachable, and what to do. */
  reason: string | null;
  info: { version?: string; api?: string; os?: string; arch?: string; kernel?: string } | null;
};

export type DockerView = { engine: DockerEngine; containers: DockerContainer[]; images: DockerImage[] };

/** Green for running, amber for a container on its way or held, red for one that exited with an error, grey for a clean exit. */
export const containerTone = (c: Pick<DockerContainer, 'state' | 'status'>): Tone => {
  switch (c.state) {
    case 'running':
      return /\(unhealthy\)/.test(c.status) ? 'bad' : /\(health: starting\)/.test(c.status) ? 'warn' : 'ok';
    case 'restarting':
    case 'paused':
      return 'warn';
    case 'created':
      return 'info';
    case 'exited': {
      const m = /Exited \((-?\d+)\)/.exec(c.status);
      return m && m[1] !== '0' ? 'bad' : 'idle';
    }
    default:
      return 'bad';
  }
};

export const summarize = (list: DockerContainer[]): { running: number; stopped: number; failing: number } => ({
  running: list.filter((c) => c.state === 'running' && containerTone(c) === 'ok').length,
  stopped: list.filter((c) => c.state !== 'running' && containerTone(c) !== 'bad').length,
  failing: list.filter((c) => containerTone(c) === 'bad').length,
});

/** `8080→80/tcp` for a published port, `80/tcp` for one that is only exposed; each once. */
export const portsText = (ports: DockerPort[]): string => {
  const out: string[] = [];
  for (const p of ports) {
    if (p.private == null) continue;
    const s = p.public != null ? `${p.public}→${p.private}/${p.type}` : `${p.private}/${p.type}`;
    if (!out.includes(s)) out.push(s);
  }
  return out.join(', ');
};

/** Sizes as `docker images` prints them: powers of 1000. */
export const fmtSize = (bytes: number): string => {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  const units = ['B', 'kB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1000 && u < units.length - 1) (v /= 1000), u++;
  return `${u === 0 || v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
};

export const ageSince = (epochSeconds: number, now: number): string => ageOf(Number.isFinite(epochSeconds) ? new Date(epochSeconds * 1000).toISOString() : null, now);

/** Containers whose name, image, id or status contain every word of `q`, optionally of one state group. */
export const filterContainers = (list: DockerContainer[], q: string, group: 'all' | 'running' | 'stopped'): DockerContainer[] => {
  const words = q.toLowerCase().split(/\s+/).filter(Boolean);
  return list.filter((c) => {
    if (group === 'running' && c.state !== 'running') return false;
    if (group === 'stopped' && c.state === 'running') return false;
    const hay = `${c.name} ${c.image} ${c.id} ${c.status}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
};

export type DockerAction = 'start' | 'stop' | 'restart';

/** What to tell the user once the engine has answered; `changed` is false when the container was already so. */
export const actionDone = (name: string, action: DockerAction, changed: boolean): string => {
  if (!changed) return action === 'start' ? `${name} was already running` : `${name} was already stopped`;
  return action === 'start' ? `${name} started` : action === 'stop' ? `${name} stopped` : `${name} restarted`;
};

export type DockerContextState = 'no-docker-cli' | 'not-installed' | 'stale' | 'socket-down' | 'ready' | 'current';

/** What `GET /v1/docker/context` answers: the `fluxvm` docker CLI context, read-only. */
export type DockerContext = {
  name: string;
  state: DockerContextState;
  summary: string;
  /** The fluxctl command that fixes or advances the state; null when there is nothing to do. */
  next: string | null;
  docker_cli: boolean;
  docker_cli_hint: string | null;
  exists: boolean;
  current: boolean;
  overridden_by: string | null;
  socket: string;
  socket_answers: boolean;
  try: string;
};

/** The command to switch `docker` to FluxVM's engine: install the context if it is missing, then make it current. */
export const USE_COMMAND = 'fluxctl docker context install --use';

export const contextTone = (s: DockerContextState): Tone => (s === 'current' ? 'ok' : s === 'ready' ? 'info' : s === 'socket-down' || s === 'stale' ? 'warn' : 'idle');

export const contextLabel = (s: DockerContextState): string =>
  ({ current: 'in use', ready: 'ready', 'socket-down': 'engine not answering', stale: 'out of date', 'not-installed': 'not created', 'no-docker-cli': 'no docker command' })[s];

/** The one command the page offers for a state: `Use FluxVM for docker` unless it is already current, or the engine must be started first. */
export const contextCommand = (c: Pick<DockerContext, 'state' | 'next'>): { label: string; command: string } | null => {
  if (c.state === 'current') return null;
  if (c.state === 'socket-down') return { label: 'Start the engine', command: c.next ?? 'fluxctl docker up' };
  if (c.state === 'no-docker-cli') return null;
  return { label: 'Use FluxVM for docker', command: USE_COMMAND };
};
