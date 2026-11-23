// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Mac page: the shape of `GET /v1/mac`, how an instance is summarised, and the `fluxctl mac` command for every action.
import { type Tone, ageOf } from './kube';

export type MacPort = { host: number; guest: number };

export type MacInstance = {
  id: string;
  name: string;
  image: string | null;
  /** `creating`, `running`, `paused`, `stopped` or `failed`. */
  status: string;
  ip: string | null;
  cpus: number | null;
  memory_mib: number | null;
  /** ISO time. */
  created: string | null;
  ports: MacPort[];
  restart_policy: string;
  project: string | null;
  service: string | null;
  error: string | null;
  labels: Record<string, string>;
};

export type MacProject = { name: string; running: number; total: number; services: { service: string | null; instance: string; status: string }[] };

export type MacImage = { reference: string; digest: string; source: string; added_unix: number; size_bytes: number | null; present: boolean };

export type MacView = {
  schema: number;
  /** False on a host that is not a Mac: the page explains instead of listing. */
  supported: boolean;
  macos_limit: number;
  macos_running: number;
  projects: MacProject[];
  instances: MacInstance[];
  images: { path: string | null; exists: boolean; items: MacImage[]; note: string | null };
  logins: { path: string | null; items: { registry: string; username: string }[]; problem: string | null };
};

export type MacAction = 'start' | 'stop' | 'restart' | 'kill' | 'rm';

export const statusTone = (status: string): Tone => {
  switch (status) {
    case 'running': return 'ok';
    case 'creating': case 'paused': return 'warn';
    case 'failed': return 'bad';
    default: return 'idle';
  }
};

export const summarize = (list: MacInstance[]): { running: number; stopped: number; failed: number } => ({
  running: list.filter((i) => i.status === 'running').length,
  stopped: list.filter((i) => i.status === 'stopped' || i.status === 'paused').length,
  failed: list.filter((i) => i.status === 'failed').length,
});

/** `2222→22, 8080→80`: host port, then the guest port it reaches. */
export const portsText = (ports: MacPort[]): string => ports.map((p) => `${p.host}→${p.guest}`).join(', ');

export const memoryText = (mib: number | null): string => (mib == null ? '—' : mib >= 1024 ? `${Number.isInteger(mib / 1024) ? mib / 1024 : (mib / 1024).toFixed(1)} GiB` : `${mib} MiB`);

export const sizeText = (bytes: number | null): string => {
  if (bytes == null || !Number.isFinite(bytes)) return '—';
  const units = ['B', 'kB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1000 && u < units.length - 1) (v /= 1000), u++;
  return `${u === 0 || v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
};

export const ageText = (created: string | null, now: number): string => ageOf(created, now);

/** Instances whose name, image, ip, project or status contain every word of `q`. */
export const filterInstances = (list: MacInstance[], q: string): MacInstance[] => {
  const words = q.toLowerCase().split(/\s+/).filter(Boolean);
  return list.filter((i) => {
    const hay = `${i.name} ${i.image ?? ''} ${i.ip ?? ''} ${i.project ?? ''} ${i.status}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
};

/** The actions that make sense for an instance in this state, in the order the buttons are shown. */
export const actionsFor = (status: string): MacAction[] => {
  switch (status) {
    case 'running': return ['restart', 'stop', 'kill', 'rm'];
    case 'creating': return ['kill', 'rm'];
    case 'paused': return ['stop', 'kill', 'rm'];
    default: return ['start', 'rm'];
  }
};

export const actionLabel = (a: MacAction): string => (a === 'rm' ? 'Remove' : a[0].toUpperCase() + a.slice(1));

/** The `fluxctl` command for an action on an instance; the page shows it next to the button and the button copies it. */
export const actionCommand = (a: MacAction, name: string): string => `fluxctl mac ${a} ${name}`;

export const actionDone = (name: string, a: MacAction): string => {
  switch (a) {
    case 'start': return `${name} started`;
    case 'stop': return `${name} stopped; its disk is kept`;
    case 'restart': return `${name} restarted`;
    case 'kill': return `${name} was killed without a clean shutdown; Start boots it again`;
    case 'rm': return `${name} removed`;
  }
};

/** Turns the daemon's refusal into what to do next, in plain English. */
export const explainError = (name: string, message: string): string => {
  if (/macos guests|macos vms|licen[sc]e|already runs/i.test(message)) return `${name}: this Mac already runs its limit of two macOS VMs (Apple's rule). Stop or remove one, then try again. Daemon said: ${message}`;
  if (/locked|login session|session/i.test(message)) return `${name}: the Mac's login session is locked or missing, and macOS VMs need it. Unlock the Mac or log in at its screen, then try again. Daemon said: ${message}`;
  if (/admin/i.test(message)) return `${name}: only an administrator can change instances. Sign in with an admin token, or run the copied fluxctl command on the Mac.`;
  return `${name}: ${message}. If it keeps failing, run fluxctl mac doctor on the Mac.`;
};

export const inspectCommand = (name: string): string => `fluxctl mac inspect ${name}`;
export const logsCommand = (name: string): string => `fluxctl mac logs --tail 300 ${name}`;
export const PS_COMMAND = 'fluxctl mac ps -a';
export const IMAGES_COMMAND = 'fluxctl mac images';
export const LOGINS_COMMAND = 'fluxctl mac logins';
export const loginCommand = (registry?: string): string => `fluxctl mac login ${registry ?? 'REGISTRY'}`;
export const COMPOSE_COMMAND = 'fluxctl mac compose ls';
export const DOCTOR_COMMAND = 'fluxctl mac doctor';

/** What to say when the Mac page is opened on a host that is not a Mac. */
export const UNSUPPORTED_NOTE = 'This host is not a Mac, so it runs no macOS instances. Open Studio on a Mac, or run fluxctl cluster status -f inventory.yaml to see your Macs.';
