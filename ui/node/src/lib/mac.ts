// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Mac page: the shape of `GET /v1/mac`, how an instance is summarised, and the `fluxctl mac` command for every action.
import { type Tone, ageOf } from './kube';
import { labelsMatch, parseQuery } from './labelFilter';

export type MacHealth = { status: 'starting' | 'healthy' | 'unhealthy'; failing_streak: number; last_check_unix: number };

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
  /** The last healthcheck result; null until `mac supervise` or `mac health` ran a probe (or no healthcheck exists). Absent on an older daemon. */
  health?: MacHealth | null;
  project: string | null;
  service: string | null;
  error: string | null;
  labels: Record<string, string>;
};

export type MacProject = { name: string; running: number; total: number; services: { service: string | null; instance: string; status: string; health?: MacHealth | null }[] };

export type MacImage = { reference: string; digest: string; source: string; added_unix: number; size_bytes: number | null; present: boolean };

/** Whether the background restarter (`fluxctl mac supervise`) is installed; without it a restart policy is only a label. */
export type MacSupervisor = { installed: boolean; label: string; install_command: string; status_command: string };

export type MacView = {
  schema: number;
  /** Absent on an older daemon. */
  supervisor?: MacSupervisor;
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

export const healthTone = (h: MacHealth | null | undefined): Tone => (h?.status === 'healthy' ? 'ok' : h?.status === 'unhealthy' ? 'bad' : h?.status === 'starting' ? 'warn' : 'idle');

/** `healthy`, `starting (1 probe failed)`, `unhealthy (3 probes failed in a row)`; empty when no probe ran. */
export const healthText = (h: MacHealth | null | undefined): string => {
  if (!h) return '';
  const n = h.failing_streak;
  return n > 0 ? `${h.status} (${n} probe${n === 1 ? '' : 's'} failed${h.status === 'unhealthy' ? ' in a row' : ' so far'})` : h.status;
};

export const healthCommand = (name: string): string => `fluxctl mac health ${name}`;

/** What to do about an unhealthy instance, in plain words; empty otherwise. */
export const healthAdvice = (i: Pick<MacInstance, 'name' | 'health'>): string =>
  i.health?.status === 'unhealthy' ? `${i.name} is unhealthy: its health command failed several times in a row. Read its log, run the command by hand with ${execCommand(i.name)}, then probe again with ${healthCommand(i.name)}. It turns healthy by itself once the command passes.` : '';

export const summarize = (list: MacInstance[]): { running: number; stopped: number; failed: number; unhealthy: number } => ({
  unhealthy: list.filter((i) => i.health?.status === 'unhealthy').length,
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
  const { words, labels } = parseQuery(q);
  return list.filter((i) => {
    if (!labelsMatch(i.labels, labels)) return false;
    const hay = `${i.name} ${i.image ?? ''} ${i.ip ?? ''} ${i.project ?? ''} ${i.status} ${i.health?.status ?? ''}`.toLowerCase();
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
/** A shell in the instance (`mac exec -it`), and what `mac attach` follows. */
export const execCommand = (name: string): string => `fluxctl mac exec -it ${name}`;
export const attachCommand = (name: string): string => `fluxctl mac attach ${name}`;
export const logsCommand = (name: string): string => `fluxctl mac logs --tail 300 ${name}`;
/** Hand a STOPPED instance to someone as one file; prints the `fluxctl import` line for the other Mac. Nothing is uploaded. */
export const shareCommand = (name: string): string => `fluxctl share ${name}`;
export const shareServeCommand = (name: string): string => `fluxctl share ${name} --serve`;
export const PS_COMMAND = 'fluxctl mac ps -a';
export const IMAGES_COMMAND = 'fluxctl mac images';
export const LOGINS_COMMAND = 'fluxctl mac logins';
export const loginCommand = (registry?: string): string => `fluxctl mac login ${registry ?? 'REGISTRY'}`;
export const COMPOSE_COMMAND = 'fluxctl mac compose ls';
/** The compose commands of a project, as fluxctl spells them: watch the files, scale a service, and ask where a fleet would run it. */
export const composeCommands = (project: string): { label: string; cmd: string }[] => [
  { label: 'Watch', cmd: `fluxctl mac compose -p ${project} watch` },
  { label: 'Scale', cmd: `fluxctl mac compose -p ${project} up --scale SERVICE=2 --dry-run` },
  { label: 'Fleet plan', cmd: `fluxctl mac compose up -p ${project} --dry-run --fleet hosts.yaml` },
  { label: 'Start order', cmd: `fluxctl mac compose -p ${project} up --dry-run` },
];

/**
 * One plain sentence about how far a project's start-up gates are: services that `depends_on` a `service_healthy` one wait for its
 * health, so say which services are healthy, still starting, or unhealthy and what to do. Empty when no service has a health probe.
 */
export const gateNote = (p: MacProject): string => {
  const probed = p.services.filter((s) => s.health);
  if (!probed.length) return '';
  const name = (s: { service: string | null; instance: string }) => s.service ?? s.instance;
  const bad = probed.filter((s) => s.health?.status === 'unhealthy').map(name);
  if (bad.length) return `${bad.join(', ')} ${bad.length === 1 ? 'is' : 'are'} unhealthy, so anything that waits for ${bad.length === 1 ? 'it' : 'them'} (depends_on with condition: service_healthy) is not started. See why with fluxctl mac health, then fluxctl mac compose down and up again.`;
  const wait = probed.filter((s) => s.health?.status === 'starting').map(name);
  if (wait.length) return `Waiting for ${wait.join(', ')} to turn healthy; services that depend on ${wait.length === 1 ? 'it' : 'them'} start after that.`;
  return `All ${probed.length} health-checked service${probed.length === 1 ? ' is' : 's are'} healthy.`;
};
export const SUPERVISE_INSTALL_COMMAND = 'fluxctl mac supervise --install';
export const SUPERVISE_STATUS_COMMAND = 'fluxctl mac supervise --status';

/** The instances whose restart policy nothing applies yet: they have a policy, and the restarter is not installed. */
export const unrestarted = (list: MacInstance[], sup: MacSupervisor | undefined): MacInstance[] => (sup && !sup.installed ? list.filter((i) => i.restart_policy !== 'no') : []);

/** One sentence for the banner above the instance list; empty when there is nothing to say. */
export const supervisorNote = (list: MacInstance[], sup: MacSupervisor | undefined): string => {
  const n = unrestarted(list, sup).length;
  if (!n) return '';
  return `${n} instance${n === 1 ? ' has' : 's have'} a restart policy, but nothing restarts ${n === 1 ? 'it' : 'them'}: the macOS daemon never restarts a VM by itself. Install the restarter once with ${SUPERVISE_INSTALL_COMMAND} (a LaunchAgent of your user, no sudo).`;
};

export const DOCTOR_COMMAND = 'fluxctl mac doctor';

/** What to say when the Mac page is opened on a host that is not a Mac. */
export const UNSUPPORTED_NOTE = 'This host is not a Mac, so it runs no macOS instances. Open Studio on a Mac, or run fluxctl cluster status -f inventory.yaml to see your Macs.';

// ---- Edit: `fluxctl mac update`

/** What the Edit panel holds: text boxes, exactly as typed. */
export type EditForm = { cpus: string; memory: string; restart: string; labels: string; restartNow: boolean; checkpoint: boolean };

/** The body of `PATCH /v1/mac/instances/{id}`. */
export type MacUpdateBody = { cpus?: number; memory_mib?: number; restart?: string; labels?: Record<string, string>; rm_labels?: string[]; restart_now?: boolean; checkpoint?: boolean };

export type MacUpdateAnswer = {
  ok: boolean;
  name: string;
  applied: string[];
  pending_restart: { change: string }[];
  restarted: boolean;
  checkpoint: string | null;
  explain: string | null;
  next: string | null;
  fluxctl: string;
};

export const RESTART_POLICIES = ['no', 'always', 'unless-stopped', 'on-failure'];

/** `8192` -> `8g`, `1536` -> `1536m`: the form Studio shows and `fluxctl --memory` takes. */
export const memoryField = (mib: number | null): string => (mib == null ? '' : mib % 1024 === 0 ? `${mib / 1024}g` : `${mib}m`);

/** `8g`, `16GiB`, `512m` or a bare number of MiB -> MiB, or null when it is not a size. */
export const parseMemory = (text: string): number | null => {
  const m = /^\s*(\d+)\s*(g|gb|gib|m|mb|mib)?\s*$/i.exec(text);
  if (!m) return null;
  const n = Number(m[1]) * (m[2] && m[2].toLowerCase().startsWith('g') ? 1024 : 1);
  return n > 0 ? n : null;
};

/** The form an instance starts with: its own values, so only what you change is sent. */
export const editFormFor = (i: MacInstance): EditForm => ({
  cpus: i.cpus == null ? '' : String(i.cpus),
  memory: memoryField(i.memory_mib),
  restart: i.restart_policy || 'no',
  labels: Object.entries(i.labels).map(([k, v]) => `${k}=${v}`).join('\n'),
  restartNow: false,
  checkpoint: true,
});

/** `a=b` lines into labels; the second value is the first line that is not `KEY=VALUE`. */
export const parseLabelLines = (text: string): { labels: Record<string, string>; bad: string | null } => {
  const labels: Record<string, string> = {};
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (!line) continue;
    const eq = line.indexOf('=');
    if (eq < 1 || /\s/.test(line.slice(0, eq))) return { labels, bad: line };
    labels[line.slice(0, eq)] = line.slice(eq + 1);
  }
  return { labels, bad: null };
};

const shq = (v: string): string => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(v) ? v : `'${v.replace(/'/g, "'\\''")}'`);

/** The `fluxctl mac update` that sends this body; the same flags the CLI has. */
export const updateCommand = (name: string, b: MacUpdateBody): string => {
  const p = [`fluxctl mac update ${shq(name)}`];
  if (b.cpus != null) p.push(`--cpus ${b.cpus}`);
  if (b.memory_mib != null) p.push(`--memory ${memoryField(b.memory_mib)}`);
  if (b.restart != null) p.push(`--restart ${b.restart}`);
  for (const [k, v] of Object.entries(b.labels ?? {})) p.push(`--label ${shq(`${k}=${v}`)}`);
  for (const k of b.rm_labels ?? []) p.push(`--rm-label ${shq(k)}`);
  if (b.restart_now) p.push('--restart-now');
  if (b.restart_now && b.checkpoint === false) p.push('--no-checkpoint');
  return p.join(' ');
};

export type EditPlan = {
  body: MacUpdateBody;
  command: string;
  /** What changes, in words, for the confirmation line. */
  changes: string[];
  /** The size changes while the instance runs: Save will not apply it unless "restart now" is ticked. */
  needsRestart: boolean;
  /** Why Save is disabled, or null. */
  error: string | null;
};

const isLive = (status: string): boolean => status === 'running' || status === 'paused';

/** Compares the form with the instance and builds only the changes; the one place the page decides what to send. */
export const planEdit = (inst: MacInstance, f: EditForm): EditPlan => {
  const body: MacUpdateBody = {};
  const changes: string[] = [];
  let error: string | null = null;
  const fail = (m: string) => { error ??= m; };
  if (f.cpus.trim() !== '' && f.cpus.trim() !== String(inst.cpus)) {
    const n = Number(f.cpus);
    if (!Number.isInteger(n) || n < 1 || n > 255) fail('CPUs must be a whole number from 1 to 255.');
    else { body.cpus = n; changes.push(`CPUs ${inst.cpus ?? '?'} to ${n}`); }
  }
  if (f.memory.trim() !== '' && f.memory.trim() !== memoryField(inst.memory_mib)) {
    const mib = parseMemory(f.memory);
    if (mib == null) fail('Memory must look like 8g, 512m or a number of MiB.');
    else if (mib < 512) fail('A macOS VM needs at least 512 MiB of memory.');
    else if (mib !== inst.memory_mib) { body.memory_mib = mib; changes.push(`memory ${memoryText(inst.memory_mib)} to ${memoryText(mib)}`); }
  }
  if (f.restart !== (inst.restart_policy || 'no')) { body.restart = f.restart; changes.push(`restart policy ${inst.restart_policy || 'no'} to ${f.restart}`); }
  const { labels, bad } = parseLabelLines(f.labels);
  if (bad != null) fail(`"${bad}" is not KEY=VALUE. Write one label per line, like team=ios.`);
  const set = Object.entries(labels).filter(([k, v]) => inst.labels[k] !== v);
  if (set.length) { body.labels = Object.fromEntries(set); changes.push(`set ${set.map(([k]) => k).join(', ')}`); }
  const rm = Object.keys(inst.labels).filter((k) => !(k in labels));
  if (rm.length) { body.rm_labels = rm; changes.push(`remove ${rm.join(', ')}`); }
  const resize = body.cpus != null || body.memory_mib != null;
  const needsRestart = resize && isLive(inst.status);
  if (resize && needsRestart && f.restartNow) {
    body.restart_now = true;
    if (!f.checkpoint) body.checkpoint = false;
  }
  if (!changes.length && !error) error = 'Nothing is different from what the instance has now.';
  return { body, command: updateCommand(inst.name, body), changes, needsRestart, error };
};

/** The daemon's answer in plain words, for the toast under the form. */
export const updateAnswerText = (name: string, a: MacUpdateAnswer): string => {
  const parts: string[] = [];
  if (a.applied.length) parts.push(`${name}: applied ${a.applied.join('; ')}.`);
  if (a.checkpoint) parts.push(`A checkpoint was saved first (${a.checkpoint}); to go back run fluxctl mac checkpoint restore ${name} ${a.checkpoint}.`);
  if (a.restarted) parts.push(`${name} was restarted with the new size.`);
  if (a.pending_restart.length) parts.push(`Not applied, needs a restart: ${a.pending_restart.map((p) => p.change).join('; ')}.`);
  if (a.next) parts.push(a.next);
  return parts.join(' ') || `${name}: nothing to change.`;
};
